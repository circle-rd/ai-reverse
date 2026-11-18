//! A fake LiteLLM management API holding model deployments in memory.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use http::{Method, Request, Response, StatusCode, header};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{TestBody, serve_http1, text};

struct LiteState {
    key: String,
    rows: Mutex<Vec<Value>>,
    next_id: AtomicU64,
}

pub struct FakeLiteLlm {
    pub addr: SocketAddr,
    state: Arc<LiteState>,
    task: JoinHandle<()>,
}

impl FakeLiteLlm {
    pub async fn start(key: &str) -> Self {
        let state = Arc::new(LiteState {
            key: key.to_owned(),
            rows: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = state.clone();
        let task = tokio::spawn(serve_http1(listener, move |request| {
            lite_handle(request, shared.clone())
        }));
        Self { addr, state, task }
    }

    /// Adds a row as an earlier relay run (managed) or an operator (unmanaged) would have.
    pub fn seed(&self, model_name: &str, managed: bool) {
        let id = format!("seed-{}", self.state.next_id.fetch_add(1, Ordering::SeqCst));
        let info = if managed {
            json!({"id": id, "managed_by": "reverse-relay", "reverse_name": model_name})
        } else {
            json!({"id": id})
        };
        self.state.rows.lock().unwrap().push(json!({
            "model_name": model_name,
            "litellm_params": {"model": "openai/x"},
            "model_info": info,
        }));
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .state
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter_map(|row| row["model_name"].as_str().map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    pub fn row(&self, model_name: &str) -> Option<Value> {
        self.state
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|row| row["model_name"] == model_name)
            .cloned()
    }
}

impl Drop for FakeLiteLlm {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn lite_handle(
    request: Request<Incoming>,
    state: Arc<LiteState>,
) -> Result<Response<TestBody>, Infallible> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(format!("Bearer {}", state.key).as_str());
    let body = request
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    if !authorized {
        return Ok(text(StatusCode::UNAUTHORIZED, "application/json", "{}"));
    }
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);

    match (method, path.as_str()) {
        (Method::GET, "/model/info") => {
            let rows = state.rows.lock().unwrap().clone();
            Ok(text(
                StatusCode::OK,
                "application/json",
                json!({"data": rows}).to_string(),
            ))
        }
        (Method::POST, "/model/new") => {
            let id = format!("dep-{}", state.next_id.fetch_add(1, Ordering::SeqCst));
            let mut info = parsed["model_info"].clone();
            info["id"] = json!(id);
            state.rows.lock().unwrap().push(json!({
                "model_name": parsed["model_name"],
                "litellm_params": parsed["litellm_params"],
                "model_info": info,
            }));
            Ok(text(StatusCode::OK, "application/json", "{}"))
        }
        (Method::POST, "/model/delete") => {
            let target = parsed["id"].as_str().unwrap_or_default().to_owned();
            let mut rows = state.rows.lock().unwrap();
            let before = rows.len();
            rows.retain(|row| row["model_info"]["id"] != target.as_str());
            let status = if rows.len() < before {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            Ok(text(status, "application/json", "{}"))
        }
        _ => Ok(text(StatusCode::NOT_FOUND, "application/json", "{}")),
    }
}
