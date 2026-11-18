//! A fake OpenAI-compatible inference engine that records what it receives.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{TestBody, serve_http1, text};

/// One request the engine received.
#[derive(Clone, Debug)]
pub struct EngineHit {
    pub path: String,
    pub authorization: Option<String>,
    pub model: Option<String>,
    pub include_usage: Option<bool>,
    pub content_length: Option<usize>,
    pub body_len: usize,
}

struct EngineState {
    models: Mutex<Vec<String>>,
    key: String,
    hits: Mutex<Vec<EngineHit>>,
    cancelled: AtomicBool,
}

pub struct FakeEngine {
    pub addr: SocketAddr,
    state: Arc<EngineState>,
    task: JoinHandle<()>,
}

impl FakeEngine {
    /// A handle that shares this engine without owning its task.
    pub fn shared(&self) -> Self {
        Self {
            addr: self.addr,
            state: self.state.clone(),
            task: tokio::spawn(async {}),
        }
    }

    pub async fn start(key: &str, models: &[&str]) -> Self {
        let state = Arc::new(EngineState {
            models: Mutex::new(models.iter().map(|m| (*m).to_owned()).collect()),
            key: key.to_owned(),
            hits: Mutex::new(Vec::new()),
            cancelled: AtomicBool::new(false),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = state.clone();
        let task = tokio::spawn(serve_http1(listener, move |request| {
            engine_handle(request, shared.clone())
        }));
        Self { addr, state, task }
    }

    pub fn set_models(&self, models: &[&str]) {
        *self.state.models.lock().unwrap() = models.iter().map(|m| (*m).to_owned()).collect();
    }

    pub fn hits_on(&self, path: &str) -> Vec<EngineHit> {
        self.state
            .hits
            .lock()
            .unwrap()
            .iter()
            .filter(|hit| hit.path == path)
            .cloned()
            .collect()
    }

    pub fn stream_was_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::SeqCst)
    }
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn receiver_body(rx: mpsc::Receiver<Bytes>) -> TestBody {
    let frames = stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, Infallible>(Frame::data(chunk)), rx))
    });
    StreamBody::new(frames).boxed()
}

async fn engine_handle(
    request: Request<Incoming>,
    state: Arc<EngineState>,
) -> Result<Response<TestBody>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok());
    let body = request
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    state.hits.lock().unwrap().push(EngineHit {
        path: path.clone(),
        authorization: authorization.clone(),
        model: parsed["model"].as_str().map(str::to_owned),
        include_usage: parsed["stream_options"]["include_usage"].as_bool(),
        content_length,
        body_len: body.len(),
    });
    if authorization.as_deref() != Some(format!("Bearer {}", state.key).as_str()) {
        return Ok(text(
            StatusCode::UNAUTHORIZED,
            "application/json",
            r#"{"error":"Unauthorized"}"#,
        ));
    }

    match (method, path.as_str()) {
        (Method::GET, "/v1/models") => {
            let data: Vec<Value> = state
                .models
                .lock()
                .unwrap()
                .iter()
                .map(|id| json!({"id": id, "object": "model"}))
                .collect();
            let body = json!({"object": "list", "data": data}).to_string();
            Ok(text(StatusCode::OK, "application/json", body))
        }
        (Method::POST, "/v1/chat/completions") => {
            if parsed["stream"] == Value::Bool(false) {
                let body = json!({
                    "id": "chatcmpl-1",
                    "object": "chat.completion",
                    "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                    "usage": {"prompt_tokens": 11, "completion_tokens": 22, "total_tokens": 33}
                })
                .to_string();
                return Ok(text(StatusCode::OK, "application/json", body));
            }
            let slow = parsed["slow"].as_bool().unwrap_or(false);
            let include_usage = parsed["stream_options"]["include_usage"] == Value::Bool(true);
            let (tx, rx) = mpsc::channel::<Bytes>(2);
            let shared = state.clone();
            tokio::spawn(async move {
                if slow {
                    loop {
                        if tx
                            .send(Bytes::from_static(b"data: {\"tick\":1}\n\n"))
                            .await
                            .is_err()
                        {
                            shared.cancelled.store(true, Ordering::SeqCst);
                            return;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                }
                for part in ["Hello ", "from ", "engine"] {
                    let chunk = format!("data: {{\"delta\":\"{part}\"}}\n\n");
                    if tx.send(Bytes::from(chunk)).await.is_err() {
                        return;
                    }
                }
                if include_usage {
                    let usage = b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":7,\"total_tokens\":12}}\n\n";
                    if tx.send(Bytes::from_static(usage)).await.is_err() {
                        return;
                    }
                }
                let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
            });
            let mut response = Response::new(receiver_body(rx));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/event-stream"),
            );
            Ok(response)
        }
        _ => Ok(text(
            StatusCode::NOT_FOUND,
            "application/json",
            r#"{"error":"nope"}"#,
        )),
    }
}
