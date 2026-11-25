//! A fake receiver of the relay's inventory: checks the signature, records every push and
//! answers with whatever settings the test chose.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use http::{Method, Request, Response, StatusCode};
use http_body_util::BodyExt;
use hyper::body::Incoming;
use ring::hmac;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::{TestBody, serve_http1, text};

pub const INVENTORY_SECRET: &str = "inventory-secret";
pub const INVENTORY_PATH: &str = "/webhooks/relay/inventory";

/// What the receiver saw and what it will answer.
pub struct InventoryLog {
    pushes: Mutex<Vec<Value>>,
    refused: Mutex<usize>,
    answer: Mutex<(StatusCode, String)>,
}

impl InventoryLog {
    /// Every accepted push body, oldest first.
    pub fn pushes(&self) -> Vec<Value> {
        self.pushes.lock().unwrap().clone()
    }

    /// Pushes refused for a bad or stale signature.
    pub fn refused(&self) -> usize {
        *self.refused.lock().unwrap()
    }

    /// Answers every following push with `settings` (the contract's answer body).
    pub fn answer_settings(&self, settings: &Value) {
        *self.answer.lock().unwrap() = (
            StatusCode::OK,
            json!({"changed": true, "settings": settings}).to_string(),
        );
    }

    /// Answers every following push with a raw status and body.
    pub fn answer_raw(&self, status: StatusCode, body: &str) {
        *self.answer.lock().unwrap() = (status, body.to_owned());
    }
}

pub struct FakeInventory {
    pub addr: SocketAddr,
    pub log: Arc<InventoryLog>,
    task: JoinHandle<()>,
}

impl FakeInventory {
    pub async fn start() -> Self {
        let log = Arc::new(InventoryLog {
            pushes: Mutex::new(Vec::new()),
            refused: Mutex::new(0),
            answer: Mutex::new((StatusCode::OK, json!({"settings": []}).to_string())),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shared = log.clone();
        let task = tokio::spawn(serve_http1(listener, move |request| {
            handle(request, shared.clone())
        }));
        Self { addr, log, task }
    }

    pub fn url(&self) -> String {
        format!("http://{}{INVENTORY_PATH}", self.addr)
    }
}

impl Drop for FakeInventory {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn handle(
    request: Request<Incoming>,
    log: Arc<InventoryLog>,
) -> Result<Response<TestBody>, Infallible> {
    let signature = request
        .headers()
        .get("x-relay-signature")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let body = request
        .into_body()
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    if method != Method::POST || path != INVENTORY_PATH {
        return Ok(text(StatusCode::NOT_FOUND, "application/json", "{}"));
    }
    if !signed_correctly(signature.as_deref(), &body) {
        *log.refused.lock().unwrap() += 1;
        return Ok(text(StatusCode::BAD_REQUEST, "application/json", "{}"));
    }
    log.pushes
        .lock()
        .unwrap()
        .push(serde_json::from_slice(&body).unwrap_or(Value::Null));
    let (status, answer) = log.answer.lock().unwrap().clone();
    Ok(text(status, "application/json", answer))
}

/// The receiver side of `t=<unix>,v1=<hex>` over `<t>.<body>`.
fn signed_correctly(header: Option<&str>, body: &[u8]) -> bool {
    let Some(header) = header else { return false };
    let mut parts = header.split(',');
    let (Some(t), Some(v1), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Some(t), Some(v1)) = (t.strip_prefix("t="), v1.strip_prefix("v1=")) else {
        return false;
    };
    let Some(mac) = (0..v1.len())
        .step_by(2)
        .map(|i| {
            v1.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
        })
        .collect::<Option<Vec<u8>>>()
    else {
        return false;
    };
    let mut signed = format!("{t}.").into_bytes();
    signed.extend_from_slice(body);
    let key = hmac::Key::new(hmac::HMAC_SHA256, INVENTORY_SECRET.as_bytes());
    hmac::verify(&key, &signed, &mac).is_ok()
}
