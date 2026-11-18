//! Pairs the agent with the relay: a person approves it in a browser, the agent receives a credential.

use std::time::Duration;

use reverse_core::secret::SecretString;
use reverse_core::shutdown::triggered;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::config::Config;
use crate::credential;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_DELAY: Duration = Duration::from_secs(10);

/// Why a pairing attempt ended without a credential.
#[derive(Debug, thiserror::Error)]
enum PairError {
    #[error("relay request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("relay answered {0}")]
    Status(reqwest::StatusCode),
    #[error("the pairing expired")]
    Expired,
    #[error("the pairing was denied")]
    Denied,
    #[error("cannot save the credential: {0}")]
    Save(#[from] std::io::Error),
}

#[derive(Deserialize)]
struct Started {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    interval: u64,
}

#[derive(Deserialize)]
struct Polled {
    status: String,
    token: Option<String>,
    namespace: Option<String>,
}

/// Pairs until a credential is obtained and saved; `None` when shutdown came first.
pub async fn acquire(
    config: &Config,
    shutdown: &mut watch::Receiver<bool>,
) -> Option<SecretString> {
    let http = match reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(http) => http,
        Err(error) => {
            tracing::error!(%error, "enroll.client_failed");
            return None;
        }
    };
    let base = format!("{}://{}", config.relay.scheme(), config.relay.authority());
    loop {
        let attempt = tokio::select! {
            result = pair_once(&http, &base, config) => result,
            () = triggered(shutdown) => return None,
        };
        match attempt {
            Ok(token) => return Some(token),
            Err(error) => tracing::warn!(%error, "enroll.failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(RETRY_DELAY) => {}
            () = triggered(shutdown) => return None,
        }
    }
}

async fn pair_once(
    http: &reqwest::Client,
    base: &str,
    config: &Config,
) -> Result<SecretString, PairError> {
    let response = http
        .post(format!("{base}/enroll/start"))
        .json(&json!({ "label": config.label }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(PairError::Status(response.status()));
    }
    let started: Started = response.json().await?;
    tracing::info!(
        url = %started.verification_uri_complete,
        code = %started.user_code,
        "enroll.waiting_for_approval"
    );
    eprintln!(
        "reverse-agent: to authorize this agent open {} and check that the code is {}",
        started.verification_uri_complete, started.user_code
    );

    let interval = Duration::from_secs(started.interval.max(1));
    loop {
        tokio::time::sleep(interval).await;
        let response = http
            .post(format!("{base}/enroll/poll"))
            .json(&json!({ "device_code": started.device_code }))
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::TOO_MANY_REQUESTS => continue,
            reqwest::StatusCode::GONE => return Err(PairError::Expired),
            status if !status.is_success() => return Err(PairError::Status(status)),
            _ => {}
        }
        let polled: Polled = response.json().await?;
        match (polled.status.as_str(), polled.token) {
            ("approved", Some(token)) => {
                let token = SecretString::new(token);
                credential::save(&config.credential_file, &token)?;
                tracing::info!(namespace = ?polled.namespace, "enroll.approved");
                return Ok(token);
            }
            ("denied", _) => return Err(PairError::Denied),
            _ => {}
        }
    }
}
