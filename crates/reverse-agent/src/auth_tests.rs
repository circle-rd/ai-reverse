use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use http::{Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use reverse_core::ids::AgentId;
use reverse_core::secret::SecretString;
use serde_json::json;
use tokio::net::TcpListener;

use super::*;
use crate::credential::Credential;

type Reply = Response<Full<bytes::Bytes>>;

fn config(issuer: &str, resource: &str, file: &Path) -> Config {
    let pairs = [
        ("AGENT_RELAY_URL", "http://127.0.0.1:8443".to_owned()),
        ("AGENT_UPSTREAM_URL", "http://127.0.0.1:30001".to_owned()),
        ("AGENT_AUTH_ISSUER", issuer.to_owned()),
        ("AGENT_AUTH_CLIENT_ID", "reverse-agent".to_owned()),
        ("AGENT_AUTH_RESOURCE", resource.to_owned()),
        ("AGENT_CREDENTIAL_FILE", file.display().to_string()),
    ];
    let map: std::collections::HashMap<&str, String> = pairs.into();
    Config::from_env(&reverse_core::config::Env::new(move |key| {
        map.get(key).cloned()
    }))
    .unwrap()
}

fn token(access: Access) -> String {
    match access {
        Access::Token(token) => token.expose().to_owned(),
        _ => panic!("expected an access token"),
    }
}

fn save_credential(file: &Path, refresh: &str) {
    credential::save(
        file,
        &Credential {
            agent_id: AgentId::parse("0123456789abcdef").unwrap(),
            refresh_token: SecretString::new(refresh.to_owned()),
        },
    )
    .unwrap();
}

fn json_reply(body: serde_json::Value) -> Reply {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Full::new(bytes::Bytes::from(body.to_string())))
        .unwrap()
}

async fn answer(request: Request<Incoming>, addr: SocketAddr, polls: Arc<AtomicU32>) -> Reply {
    let base = format!("http://{addr}");
    let (parts, body) = request.into_parts();
    let bytes = body
        .collect()
        .await
        .map_or_else(|_| Default::default(), |b| b.to_bytes());
    let form = String::from_utf8_lossy(&bytes).to_string();
    match (parts.method, parts.uri.path()) {
        (Method::GET, "/.well-known/openid-configuration") => json_reply(json!({
            "issuer": base,
            "device_authorization_endpoint": format!("{base}/device"),
            "token_endpoint": format!("{base}/token"),
        })),
        (Method::POST, "/device") => json_reply(json!({
            "device_code": "device-1",
            "user_code": "ABCD-EFGH",
            "verification_uri_complete": format!("{base}/device?code=ABCD-EFGH"),
            "interval": 1,
            "expires_in": 60,
        })),
        (Method::POST, "/token") if form.contains("device_code") => {
            if polls.fetch_add(1, Ordering::Relaxed) == 0 {
                json_reply(json!({ "error": "authorization_pending" }))
            } else {
                json_reply(json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-1",
                    "token_type": "Bearer",
                }))
            }
        }
        (Method::POST, "/token") if form.contains("refresh_token=refresh-1") => json_reply(json!({
            "access_token": "access-2",
            "refresh_token": "refresh-2",
            "token_type": "Bearer",
        })),
        (Method::POST, "/token") => {
            let mut response = json_reply(json!({ "error": "invalid_grant" }));
            *response.status_mut() = StatusCode::BAD_REQUEST;
            response
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(bytes::Bytes::new()))
            .unwrap(),
    }
}

async fn spawn_idp() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let polls = Arc::new(AtomicU32::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let polls = polls.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<Incoming>| {
                    let polls = polls.clone();
                    async move { Ok::<_, Infallible>(answer(request, addr, polls).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    addr
}

#[tokio::test]
async fn the_device_flow_saves_a_rotating_credential_and_renews_it() {
    let addr = spawn_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credential");
    let config = config(&format!("http://{addr}"), "http://127.0.0.1:8443", &file);
    let mut session = Session::new(&config).unwrap();
    session.load();
    let (_stop, mut shutdown) = watch::channel(false);

    assert!(matches!(
        session.access(&mut shutdown).await,
        Access::Rejected
    ));
    assert!(session.enroll(&mut shutdown).await);
    assert_eq!(token(session.access(&mut shutdown).await), "access-1");
    let stored = credential::load(&file).unwrap().unwrap();
    assert_eq!(stored.refresh_token.expose(), "refresh-1");

    assert_eq!(token(session.renew(&mut shutdown).await), "access-2");
    let stored = credential::load(&file).unwrap().unwrap();
    assert_eq!(stored.refresh_token.expose(), "refresh-2");

    session.forget();
    assert!(credential::load(&file).unwrap().is_none());
}

#[tokio::test]
async fn a_saved_credential_renews_after_a_restart_without_enrolling() {
    let addr = spawn_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credential");
    save_credential(&file, "refresh-1");
    let config = config(&format!("http://{addr}"), "http://127.0.0.1:8443", &file);
    let mut session = Session::new(&config).unwrap();
    session.load();
    let (_stop, mut shutdown) = watch::channel(false);

    assert!(session.agent_id().is_some());
    assert_eq!(token(session.access(&mut shutdown).await), "access-2");
    let stored = credential::load(&file).unwrap().unwrap();
    assert_eq!(stored.refresh_token.expose(), "refresh-2");
}

#[tokio::test]
async fn a_refused_refresh_token_is_reported_as_rejected() {
    let addr = spawn_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credential");
    save_credential(&file, "refresh-expired");
    let config = config(&format!("http://{addr}"), "http://127.0.0.1:8443", &file);
    let mut session = Session::new(&config).unwrap();
    session.load();
    let (_stop, mut shutdown) = watch::channel(false);

    assert!(matches!(
        session.renew(&mut shutdown).await,
        Access::Rejected
    ));
    assert!(credential::load(&file).unwrap().is_some());
}

#[tokio::test]
async fn an_unreachable_provider_keeps_the_credential() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("credential");
    save_credential(&file, "refresh-1");
    let config = config("http://127.0.0.1:1", "http://127.0.0.1:8443", &file);
    let mut session = Session::new(&config).unwrap();
    session.load();
    let (_stop, mut shutdown) = watch::channel(false);

    assert!(session.agent_id().is_some());
    assert!(matches!(
        session.access(&mut shutdown).await,
        Access::Unavailable
    ));
    let stored = credential::load(&file).unwrap().unwrap();
    assert_eq!(stored.refresh_token.expose(), "refresh-1");
}

#[test]
fn agent_ids_are_validated_after_generation() {
    let id = generate_agent_id().unwrap();
    assert_eq!(id.as_str().len(), AGENT_ID_LEN);
}
