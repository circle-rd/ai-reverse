//! Device authorization against the identity provider and renewal of the access token.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
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
const SAVE_ATTEMPTS: u32 = 3;
const SAVE_RETRY_DELAY: Duration = Duration::from_secs(1);

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

/// Outcome of asking the identity provider for an access token.
pub enum Access {
    /// A usable access token.
    Token(SecretString),
    /// The provider refused the saved refresh token (`invalid_grant`): enrolment must run again.
    Rejected,
    /// The provider could not be reached or answered unexpectedly; the credential is kept.
    Unavailable,
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

    /// Returns a usable access token, renewing it when none is cached.
    pub async fn access(&mut self, shutdown: &mut watch::Receiver<bool>) -> Access {
        if let Some(access) = &self.access {
            return Access::Token(access.clone());
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

    /// Uses the refresh token to obtain a new access token. Only a refusal with
    /// `invalid_grant` is `Rejected`; every other failure keeps the credential for a later retry.
    pub async fn renew(&mut self, shutdown: &mut watch::Receiver<bool>) -> Access {
        let Some(refresh) = self.credential.as_ref().map(|c| c.refresh_token.clone()) else {
            return Access::Rejected;
        };
        let endpoints = match self.endpoints().await {
            Ok(endpoints) => endpoints,
            Err(error) => {
                tracing::warn!(%error, "auth.endpoints_failed");
                return Access::Unavailable;
            }
        };
        let sent = tokio::select! {
            result = self
                .http
                .post(&endpoints.token)
                .form(&[
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh.expose()),
                    ("client_id", self.client_id.as_str()),
                ])
                .send() => result,
            () = triggered(shutdown) => return Access::Unavailable,
        };
        let response = match sent {
            Ok(response) => response,
            Err(error) => {
                tracing::warn!(%error, "auth.refresh_failed");
                return Access::Unavailable;
            }
        };
        let status = response.status();
        if !status.is_success() {
            return refusal(status, response).await;
        }
        let Ok(answer) = response.json::<TokenAnswer>().await else {
            tracing::warn!("auth.refresh_unexpected_body");
            return Access::Unavailable;
        };
        let Some(access) = answer.access_token.map(SecretString::new) else {
            tracing::warn!("auth.refresh_unexpected_body");
            return Access::Unavailable;
        };
        if let Some(rotated) = answer.refresh_token
            && let Some(credential) = self.credential.as_mut()
        {
            credential.refresh_token = SecretString::new(rotated);
            save_with_retry(&self.credential_file, credential).await;
        }
        self.access = Some(access.clone());
        tracing::info!("auth.refreshed");
        Access::Token(access)
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

/// Classifies a refused refresh. Only `invalid_grant` means the saved credential no longer works;
/// server errors, rate limits and anything unexpected leave it in place.
async fn refusal(status: StatusCode, response: reqwest::Response) -> Access {
    let error = response
        .json::<TokenAnswer>()
        .await
        .ok()
        .and_then(|answer| answer.error);
    if error.as_deref() == Some("invalid_grant") {
        tracing::info!(%status, "auth.refresh_rejected");
        return Access::Rejected;
    }
    tracing::warn!(
        %status,
        error = error.as_deref().unwrap_or("none"),
        "auth.refresh_unavailable"
    );
    Access::Unavailable
}

/// Persists a rotated refresh token. The previous one is already invalid at the provider, so a
/// failed write is retried and logged; a restart before a successful write falls back to enrolment.
async fn save_with_retry(path: &Path, credential: &Credential) {
    for attempt in 1..=SAVE_ATTEMPTS {
        match credential::save(path, credential) {
            Ok(()) => return,
            Err(error) => {
                tracing::error!(%error, attempt, "credential.save_failed");
                if attempt < SAVE_ATTEMPTS {
                    tokio::time::sleep(SAVE_RETRY_DELAY).await;
                }
            }
        }
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
#[path = "auth_tests.rs"]
mod tests;
