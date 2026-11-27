//! Fakes and a launcher wiring a real agent and relay to a fake engine and a fake LiteLLM.

#![allow(dead_code)] // each test binary uses a different subset of the harness

pub mod engine;
pub mod hub;
pub mod idp;
pub mod inventory;
pub mod litellm;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, Method, Request, Response, StatusCode, header};
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use reverse_core::config::Env;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinHandle;

pub use engine::FakeEngine;
pub use hub::FakeHub;
pub use idp::FakeIdp;
pub use inventory::{FakeInventory, INVENTORY_SECRET};
pub use litellm::FakeLiteLlm;

pub type TestBody = BoxBody<Bytes, Infallible>;

const WAIT: Duration = Duration::from_secs(20);
/// Audience the fake provider mints tokens for; the relay's default audience.
pub const AUDIENCE: &str = "http://relay.test";

/// Polls `probe` until it yields a value, failing the test after a bounded wait.
pub async fn eventually<T, Fut>(what: &str, mut probe: impl FnMut() -> Fut) -> T
where
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

pub fn text(
    status: StatusCode,
    content_type: &'static str,
    body: impl Into<Bytes>,
) -> Response<TestBody> {
    let mut response = Response::new(Full::new(body.into()).boxed());
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type),
    );
    response
}

pub async fn serve_http1<H, Fut>(listener: TcpListener, handler: H)
where
    H: Fn(Request<Incoming>) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<Response<TestBody>, Infallible>> + Send + 'static,
{
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let _ = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service_fn(handler))
                .await;
        });
    }
}

// ── Launcher ──

pub struct Options {
    pub agent_key: &'static str,
    pub models: &'static [&'static str],
    pub extra_models: Option<&'static str>,
    pub model_tags: Option<&'static str>,
    pub max_tunnels_per_user: Option<&'static str>,
    pub max_tunnels: Option<&'static str>,
    /// Starts a fake inventory receiver and points the relay at it.
    pub inventory: bool,
    pub host_identity: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            agent_key: ENGINE_KEY,
            models: &["qwen3.8-27b"],
            extra_models: None,
            // The hub does not know this internal id, so the operator declares it.
            model_tags: Some("qwen3.8-27b=text"),
            max_tunnels_per_user: None,
            max_tunnels: None,
            inventory: false,
            host_identity: false,
        }
    }
}

pub const ENGINE_KEY: &str = "engine-key";
pub const LITELLM_KEY: &str = "master-key";
pub const API_KEY: &str = "relay-api-key";

pub struct Stack {
    pub engine: FakeEngine,
    pub litellm: FakeLiteLlm,
    pub hub: FakeHub,
    pub idp: FakeIdp,
    pub inventory: Option<FakeInventory>,
    pub relay_api: SocketAddr,
    pub relay_tunnel: SocketAddr,
    pub credential_file: std::path::PathBuf,
    agent_pairs: Vec<(&'static str, String)>,
    _dir: tempfile::TempDir,
    relay_stop: watch::Sender<bool>,
    relay_task: JoinHandle<()>,
    agent_stop: watch::Sender<bool>,
    agent_task: JoinHandle<()>,
}

/// The second node of the test account, with its own engine.
pub struct SecondAgent {
    pub engine: FakeEngine,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl SecondAgent {
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        tokio::time::timeout(WAIT, self.task)
            .await
            .unwrap()
            .unwrap();
    }
}

fn env_of(pairs: Vec<(&'static str, String)>) -> Env<impl Fn(&str) -> Option<String> + use<>> {
    let map: HashMap<&'static str, String> = pairs.into_iter().collect();
    Env::new(move |key| map.get(key).cloned())
}

impl Stack {
    pub async fn start(options: Options) -> Self {
        Self::start_with(options, |_| {}).await
    }

    /// Like [`Stack::start`], with a hook to prepare LiteLLM before the relay boots.
    pub async fn start_with(options: Options, prepare: impl FnOnce(&FakeLiteLlm)) -> Self {
        let engine = FakeEngine::start(ENGINE_KEY, options.models).await;
        let litellm = FakeLiteLlm::start(LITELLM_KEY).await;
        let hub = FakeHub::start().await;
        let idp = FakeIdp::start("tester", AUDIENCE).await;
        let inventory = if options.inventory {
            Some(FakeInventory::start().await)
        } else {
            None
        };
        prepare(&litellm);

        let dir = tempfile::tempdir().unwrap();
        let credential_file = dir.path().join("credential");
        let mut relay_pairs = vec![
            ("RELAY_TUNNEL_LISTEN", "127.0.0.1:0".into()),
            ("RELAY_API_LISTEN", "127.0.0.1:0".into()),
            ("RELAY_LITELLM_URL", format!("http://{}", litellm.addr)),
            ("RELAY_LITELLM_MASTER_KEY", LITELLM_KEY.into()),
            ("RELAY_PUBLIC_URL", "http://relay.test".into()),
            ("RELAY_PUBLIC_API_BASE", "http://relay.test".into()),
            ("RELAY_API_KEY", API_KEY.into()),
            ("RELAY_AUTH_ISSUER", format!("http://{}", idp.addr)),
            ("RELAY_MODELS_POLL_SECS", "1".into()),
            ("RELAY_DEREGISTER_GRACE_SECS", "2".into()),
            ("RELAY_RECONCILE_INTERVAL_SECS", "1".into()),
            ("RELAY_CATALOGUE_CLIENT_ID", "reverse-relay".into()),
            ("RELAY_CATALOGUE_CLIENT_SECRET", "relay-secret".into()),
        ];
        if let Some(inventory) = &inventory {
            relay_pairs.extend([
                ("RELAY_INVENTORY_URL", inventory.url()),
                ("RELAY_INVENTORY_SECRET", INVENTORY_SECRET.to_owned()),
                ("RELAY_INVENTORY_RELAY_ID", "relay-test".to_owned()),
                ("RELAY_INVENTORY_INTERVAL_SECS", "1".to_owned()),
            ]);
        }
        if let Some(cap) = options.max_tunnels_per_user {
            relay_pairs.push(("RELAY_MAX_TUNNELS_PER_USER", cap.to_owned()));
        }
        if let Some(cap) = options.max_tunnels {
            relay_pairs.push(("RELAY_MAX_TUNNELS", cap.to_owned()));
        }
        let relay_config = reverse_relay::Config::from_env(&env_of(relay_pairs)).unwrap();
        let relay = reverse_relay::Relay::bind(relay_config).await.unwrap();
        let relay_api = relay.api_addr().unwrap();
        let relay_tunnel = relay.tunnel_addr().unwrap();
        let (relay_stop, relay_stopped) = reverse_core::shutdown::channel();
        let relay_task = tokio::spawn(relay.serve(relay_stopped));

        let mut agent_pairs = vec![
            ("AGENT_RELAY_URL", format!("http://{relay_tunnel}")),
            ("AGENT_AUTH_ISSUER", format!("http://{}", idp.addr)),
            ("AGENT_AUTH_CLIENT_ID", "reverse-agent".to_owned()),
            ("AGENT_AUTH_RESOURCE", AUDIENCE.to_owned()),
            (
                "AGENT_CREDENTIAL_FILE",
                credential_file.display().to_string(),
            ),
            ("AGENT_UPSTREAM_URL", format!("http://{}", engine.addr)),
            ("AGENT_UPSTREAM_API_KEY", options.agent_key.to_owned()),
            ("AGENT_MODELS_POLL_SECS", "1".to_owned()),
            ("AGENT_BENCHMARK", "false".to_owned()),
            ("AGENT_HF_METADATA", "true".to_owned()),
            ("AGENT_HF_URL", format!("http://{}", hub.addr)),
        ];
        if let Some(extra) = options.extra_models {
            agent_pairs.push(("AGENT_EXTRA_MODELS", extra.to_owned()));
        }
        if let Some(tags) = options.model_tags {
            agent_pairs.push(("AGENT_MODEL_TAGS", tags.to_owned()));
        }
        if options.host_identity {
            agent_pairs.extend([
                (
                    "AGENT_MACHINE_ID",
                    "0123456789abcdef0123456789abcdef".to_owned(),
                ),
                ("AGENT_HOSTNAME", "test-machine".to_owned()),
            ]);
        }
        let agent_config = reverse_agent::Config::from_env(&env_of(agent_pairs.clone())).unwrap();
        let (agent_stop, agent_stopped) = reverse_core::shutdown::channel();
        let agent_task = tokio::spawn(reverse_agent::run(agent_config, agent_stopped));

        Self {
            engine,
            litellm,
            hub,
            idp,
            inventory,
            relay_api,
            relay_tunnel,
            credential_file,
            agent_pairs,
            _dir: dir,
            relay_stop,
            relay_task,
            agent_stop,
            agent_task,
        }
    }

    /// Connects one more node of the same account, sharing the stack's engine.
    pub async fn spawn_agent(&self, index: usize) -> SecondAgent {
        let config = reverse_agent::Config::from_env(&env_of(vec![
            ("AGENT_RELAY_URL", format!("http://{}", self.relay_tunnel)),
            ("AGENT_AUTH_ISSUER", format!("http://{}", self.idp.addr)),
            ("AGENT_AUTH_CLIENT_ID", "reverse-agent".to_owned()),
            ("AGENT_AUTH_RESOURCE", AUDIENCE.to_owned()),
            (
                "AGENT_CREDENTIAL_FILE",
                self.credential_file
                    .with_file_name(format!("credential-{index}"))
                    .display()
                    .to_string(),
            ),
            ("AGENT_UPSTREAM_URL", format!("http://{}", self.engine.addr)),
            ("AGENT_UPSTREAM_API_KEY", ENGINE_KEY.into()),
            ("AGENT_MODELS_POLL_SECS", "1".into()),
            ("AGENT_BENCHMARK", "false".into()),
            ("AGENT_HF_METADATA", "true".into()),
            ("AGENT_HF_URL", format!("http://{}", self.hub.addr)),
        ]))
        .unwrap();
        let (stop, stopped) = reverse_core::shutdown::channel();
        let task = tokio::spawn(reverse_agent::run(config, stopped));
        SecondAgent {
            engine: self.engine.shared(),
            stop,
            task,
        }
    }

    /// Connects the account's second node to its own engine.
    pub async fn start_second_agent(&self, models: &[&str]) -> SecondAgent {
        let engine = FakeEngine::start(ENGINE_KEY, models).await;
        let credential_file = self.credential_file.with_file_name("credential-two");
        let mut pairs = vec![
            ("AGENT_RELAY_URL", format!("http://{}", self.relay_tunnel)),
            ("AGENT_AUTH_ISSUER", format!("http://{}", self.idp.addr)),
            ("AGENT_AUTH_CLIENT_ID", "reverse-agent".to_owned()),
            ("AGENT_AUTH_RESOURCE", AUDIENCE.to_owned()),
            (
                "AGENT_CREDENTIAL_FILE",
                credential_file.display().to_string(),
            ),
            ("AGENT_UPSTREAM_URL", format!("http://{}", engine.addr)),
            ("AGENT_UPSTREAM_API_KEY", ENGINE_KEY.into()),
            ("AGENT_MODELS_POLL_SECS", "1".into()),
            ("AGENT_BENCHMARK", "false".into()),
            ("AGENT_HF_METADATA", "true".into()),
            ("AGENT_HF_URL", format!("http://{}", self.hub.addr)),
        ];
        pairs.extend(
            self.agent_pairs
                .iter()
                .filter(|(key, _)| *key == "AGENT_MACHINE_ID" || *key == "AGENT_HOSTNAME")
                .cloned(),
        );
        let config = reverse_agent::Config::from_env(&env_of(pairs)).unwrap();
        let (stop, stopped) = reverse_core::shutdown::channel();
        let task = tokio::spawn(reverse_agent::run(config, stopped));
        SecondAgent { engine, stop, task }
    }

    /// Restarts the agent with the same saved credential, as a process restart would.
    pub async fn restart_agent(&mut self) {
        self.stop_agent().await;
        let config = reverse_agent::Config::from_env(&env_of(self.agent_pairs.clone())).unwrap();
        let (stop, stopped) = reverse_core::shutdown::channel();
        self.agent_stop = stop;
        self.agent_task = tokio::spawn(reverse_agent::run(config, stopped));
    }

    /// Stops only the agent, as a dropped connection would.
    pub async fn stop_agent(&mut self) {
        let _ = self.agent_stop.send(true);
        let task = std::mem::replace(&mut self.agent_task, tokio::spawn(async {}));
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    }

    /// Stops everything and waits for both to finish.
    pub async fn shutdown(self) -> FakeLiteLlm {
        let _ = self.agent_stop.send(true);
        let _ = self.relay_stop.send(true);
        tokio::time::timeout(WAIT, self.agent_task)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(WAIT, self.relay_task)
            .await
            .unwrap()
            .unwrap();
        self.litellm
    }
}

// ── HTTP client helpers ──

pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

fn client() -> Client<hyper_util::client::legacy::connect::HttpConnector, Full<Bytes>> {
    Client::builder(TokioExecutor::new()).build_http()
}

pub fn request(addr: SocketAddr, method: Method, path: &str, body: &str) -> Request<Full<Bytes>> {
    Request::builder()
        .method(method)
        .uri(format!("http://{addr}{path}"))
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {API_KEY}"))
        .body(Full::new(Bytes::from(body.to_owned())))
        .unwrap()
}

pub async fn send(addr: SocketAddr, method: Method, path: &str, body: &str) -> Reply {
    let response = client()
        .request(request(addr, method, path, body))
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

/// A GET with a caller-chosen bearer key (or none).
pub async fn get_with_key(addr: SocketAddr, path: &str, key: Option<&str>) -> Reply {
    let mut builder = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{addr}{path}"));
    if let Some(key) = key {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {key}"));
    }
    let response = client()
        .request(builder.body(Full::new(Bytes::new())).unwrap())
        .await
        .unwrap();
    let (parts, body) = response.into_parts();
    Reply {
        status: parts.status,
        headers: parts.headers,
        body: body.collect().await.unwrap().to_bytes(),
    }
}

/// Starts a request and returns the live response so a test can read part of it and drop it.
pub async fn send_streaming(addr: SocketAddr, path: &str, body: &str) -> Response<Incoming> {
    client()
        .request(request(addr, Method::POST, path, body))
        .await
        .unwrap()
}
