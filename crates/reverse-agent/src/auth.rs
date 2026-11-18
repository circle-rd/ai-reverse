//! Device authorization against the identity provider and renewal of the access token.

use std::path::PathBuf;
use std::time::Duration;

use reverse_core::ids::{AGENT_ID_LEN, AgentId};
use reverse_core::secret::SecretString;
use reverse_core::shutdown::triggered;
use serde::Deserialize;
use tokio::sync::watch;

use crate::config::Config;
use crate::credential::{self, Credential};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
const SLOW_DOWN_STEP: u64 = 5;

/// Why an authorization attempt ended without an access token.
#[derive(Debug, thiserror::Error)]
enum AuthError {
    #[error("identity provider request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("identity provider answered {0}")]
    Status(reqwest::StatusCode),
    #[error("identity provider answered an unexpected body: {0}")]
    Body(String),
    #[error("the discovery document does not name the configured issuer")]
    IssuerMismatch,
    #[error("the authorization expired")]
    Expired,
    #[error("the authorization was denied")]
    Denied,
    #[error("the identity provider issued no refresh token")]
    NoRefreshToken,
    #[error("cannot save the credential: {0}")]
    Save(#[from] std::io::Error),
    #[error("cannot generate an agent id: {0}")]
    AgentId(getrandom::Error),
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    device_authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct DeviceGrant {
    device_code: String,
    user_code: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    #[serde(default)]
    verification_uri: Option<String>,
    #[serde(default = "default_interval")]
    interval: u64,
    #[serde(default = "default_expiry")]
    expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn default_expiry() -> u64 {
    600
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: Option<String>,
    refresh_token: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone)]
struct Endpoints {
    device_authorization: String,
    token: String,
}

/// Holds the credential kept between restarts and the short-lived access token.
pub struct Session {
    http: reqwest::Client,
    issuer: String,
    client_id: String,
    resource: String,
    scope: String,
    credential_file: PathBuf,
    credential: Option<Credential>,
    access: Option<SecretString>,
    endpoints: Option<Endpoints>,
}

impl Session {
    /// Builds a session from the configuration; the saved credential is loaded separately.
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            issuer: config.auth_issuer.origin(),
            client_id: config.auth_client_id.clone(),
            resource: config.auth_resource.origin(),
            scope: config.auth_scope.clone(),
            credential_file: config.credential_file.clone(),
            credential: None,
            access: None,
            endpoints: None,
        })
    }

    /// Loads a credential saved by a previous run; `None` when the agent never enrolled.
    pub fn load(&mut self) {
        match credential::load(&self.credential_file) {
            Ok(credential) => self.credential = credential,
            Err(error) => tracing::warn!(%error, "credential.unreadable"),
        }
    }

    /// The stable agent id, once a credential exists.
    #[must_use]
    pub fn agent_id(&self) -> Option<AgentId> {
        self.credential.as_ref().map(|c| c.agent_id.clone())
    }

    /// Returns a usable access token, refreshing it when only the refresh token is left.
    pub async fn access(&mut self, shutdown: &mut watch::Receiver<bool>) -> Option<SecretString> {
        if let Some(access) = &self.access {
            return Some(access.clone());
        }
        self.renew(shutdown).await
    }

    /// Forgets the saved credential so the next attempt runs the device flow.
    pub fn forget(&mut self) {
        credential::forget(&self.credential_file);
        self.credential = None;
        self.access = None;
    }

    /// Runs the device flow until an access token is issued; `false` when shutdown came first.
    pub async fn enroll(&mut self, shutdown: &mut watch::Receiver<bool>) -> bool {
        loop {
            let attempt = tokio::select! {
                result = self.enroll_once() => result,
                () = triggered(shutdown) => return false,
            };
            match attempt {
                Ok(access) => {
                    self.access = Some(access);
                    return true;
                }
                Err(error) => tracing::warn!(%error, "auth.enroll_failed"),
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(10)) => {}
                () = triggered(shutdown) => return false,
            }
        }
    }

    /// Uses the refresh token to obtain a new access token; `None` when it is no longer valid.
    pub async fn renew(&mut self, shutdown: &mut watch::Receiver<bool>) -> Option<SecretString> {
        let refresh = self.credential.as_ref()?.refresh_token.clone();
        let endpoints = match self.endpoints().await {
            Ok(endpoints) => endpoints,
            Err(error) => {
                tracing::warn!(%error, "auth.endpoints_failed");
                return None;
            }
        };
        let answer = tokio::select! {
            result = self.http
                .post(&endpoints.token)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh.expose()),
                    ("client_id", self.client_id.as_str()),
                ])
                .send() => result,
            () = triggered(shutdown) => return None,
        };
        let answer = match answer {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, "auth.refresh_failed");
                return None;
            }
        };
        if !answer.status().is_success() {
            tracing::info!(status = %answer.status(), "auth.refresh_rejected");
            return None;
        }
        let answer: TokenAnswer = answer.json().await.ok()?;
        let access = answer.access_token.map(SecretString::new)?;
        if let Some(rotated) = answer.refresh_token
            && let Some(credential) = self.credential.as_mut()
        {
            credential.refresh_token = SecretString::new(rotated);
            if let Err(error) = credential::save(&self.credential_file, credential) {
                tracing::warn!(%error, "credential.save_failed");
            }
        }
        self.access = Some(access.clone());
        tracing::info!("auth.refreshed");
        Some(access)
    }

    async fn enroll_once(&mut self) -> Result<SecretString, AuthError> {
        let endpoints = self.endpoints().await?;
        let granted: DeviceGrant = self
            .http
            .post(&endpoints.device_authorization)
            .form(&[
                ("client_id", self.client_id.as_str()),
                ("scope", self.scope.as_str()),
                ("resource", self.resource.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let verification = granted
            .verification_uri_complete
            .clone()
            .or(granted.verification_uri.clone())
            .ok_or_else(|| AuthError::Body("no verification uri".into()))?;
        tracing::info!(
            url = %verification,
            code = %granted.user_code,
            "auth.waiting_for_approval"
        );
        eprintln!(
            "reverse-agent: to authorize this agent open {} and check that the code is {}",
            verification, granted.user_code
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(granted.expires_in.max(1));
        let mut interval = Duration::from_secs(granted.interval.max(1));
        loop {
            tokio::time::sleep(interval).await;
            if tokio::time::Instant::now() >= deadline {
                return Err(AuthError::Expired);
            }
            let answer = self
                .http
                .post(&endpoints.token)
                .form(&[
                    ("grant_type", DEVICE_GRANT),
                    ("device_code", granted.device_code.as_str()),
                    ("client_id", self.client_id.as_str()),
                ])
                .send()
                .await?;
            let status = answer.status();
            let body: TokenAnswer = answer
                .json()
                .await
                .map_err(|error| AuthError::Body(error.to_string()))?;
            match body.error.as_deref() {
                None if status.is_success() => {
                    let access = body
                        .access_token
                        .map(SecretString::new)
                        .ok_or_else(|| AuthError::Body("no access token".into()))?;
                    let refresh = body.refresh_token.ok_or(AuthError::NoRefreshToken)?;
                    let agent_id = generate_agent_id()?;
                    let credential = Credential {
                        agent_id: agent_id.clone(),
                        refresh_token: SecretString::new(refresh),
                    };
                    credential::save(&self.credential_file, &credential)?;
                    self.credential = Some(credential);
                    tracing::info!(agent_id = %agent_id, "auth.enrolled");
                    return Ok(access);
                }
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    interval += Duration::from_secs(SLOW_DOWN_STEP);
                }
                Some("access_denied") => return Err(AuthError::Denied),
                Some("expired_token") => return Err(AuthError::Expired),
                Some(other) => return Err(AuthError::Body(other.to_owned())),
                None => return Err(AuthError::Status(status)),
            }
        }
    }

    async fn endpoints(&mut self) -> Result<Endpoints, AuthError> {
        if self.endpoints.is_none() {
            let url = format!("{}/.well-known/openid-configuration", self.issuer);
            let discovery: Discovery = self
                .http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if normalize(&discovery.issuer) != normalize(&self.issuer) {
                return Err(AuthError::IssuerMismatch);
            }
            self.endpoints = Some(Endpoints {
                device_authorization: discovery.device_authorization_endpoint,
                token: discovery.token_endpoint,
            });
        }
        self.endpoints
            .clone()
            .ok_or_else(|| AuthError::Body("missing endpoints".into()))
    }
}

fn normalize(url: &str) -> &str {
    url.trim_end_matches('/')
}

fn generate_agent_id() -> Result<AgentId, AuthError> {
    let mut bytes = [0_u8; AGENT_ID_LEN / 2];
    getrandom::fill(&mut bytes).map_err(AuthError::AgentId)?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    AgentId::parse(&hex).map_err(|_| AuthError::Body("generated agent id is invalid".into()))
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use http::{Method, Request, Response, StatusCode, header};
    use http_body_util::{BodyExt, Full};
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    fn config(issuer: &str, resource: &str, file: &std::path::Path) -> Config {
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

    async fn idp(
        addr: SocketAddr,
        polls: Arc<AtomicU32>,
    ) -> impl Fn(
        Request<Incoming>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<Response<Full<bytes::Bytes>>, Infallible>> + Send>,
    > + Clone {
        let base = format!("http://{addr}");
        move |request: Request<Incoming>| {
            let base = base.clone();
            let polls = polls.clone();
            Box::pin(async move {
                let (parts, body) = request.into_parts();
                let bytes = body
                    .collect()
                    .await
                    .map_or_else(|_| Default::default(), |b| b.to_bytes());
                let form = String::from_utf8_lossy(&bytes).to_string();
                let reply = match (parts.method, parts.uri.path()) {
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
                    (Method::POST, "/token") if form.contains("refresh_token=refresh-1") => {
                        json_reply(json!({
                            "access_token": "access-2",
                            "refresh_token": "refresh-2",
                            "token_type": "Bearer",
                        }))
                    }
                    (Method::POST, "/token") => {
                        let mut response = json_reply(json!({ "error": "invalid_grant" }));
                        *response.status_mut() = StatusCode::BAD_REQUEST;
                        response
                    }
                    _ => Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .body(Full::new(bytes::Bytes::new()))
                        .unwrap(),
                };
                Ok(reply)
            })
        }
    }

    fn json_reply(body: serde_json::Value) -> Response<Full<bytes::Bytes>> {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(bytes::Bytes::from(body.to_string())))
            .unwrap()
    }

    #[tokio::test]
    async fn the_device_flow_saves_a_rotating_credential_and_renews_it() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let polls = Arc::new(AtomicU32::new(0));
        let handler = idp(addr, polls).await;
        tokio::spawn(async move {
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
        });

        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("credential");
        let config = config(&format!("http://{addr}"), "http://127.0.0.1:8443", &file);
        let mut session = Session::new(&config).unwrap();
        session.load();
        let (_stop, mut shutdown) = watch::channel(false);

        assert!(session.access(&mut shutdown).await.is_none());
        assert!(session.enroll(&mut shutdown).await);
        assert_eq!(
            session.access(&mut shutdown).await.unwrap().expose(),
            "access-1"
        );
        let stored = credential::load(&file).unwrap().unwrap();
        assert_eq!(stored.refresh_token.expose(), "refresh-1");

        assert_eq!(
            session.renew(&mut shutdown).await.unwrap().expose(),
            "access-2"
        );
        let stored = credential::load(&file).unwrap().unwrap();
        assert_eq!(stored.refresh_token.expose(), "refresh-2");

        session.forget();
        assert!(credential::load(&file).unwrap().is_none());
    }

    #[test]
    fn agent_ids_are_validated_after_generation() {
        let id = generate_agent_id().unwrap();
        assert_eq!(id.as_str().len(), AGENT_ID_LEN);
    }
}
