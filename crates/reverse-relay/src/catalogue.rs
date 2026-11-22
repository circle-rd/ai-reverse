//! Tells auth-service that the models LiteLLM serves changed, so its model catalogue refreshes
//! without waiting for its own periodic inventory sync.

use std::sync::Arc;
use std::time::Duration;

use http::{StatusCode, header};
use reqwest::{Client, Url, redirect};
use reverse_core::secret::SecretString;
use reverse_core::shutdown::triggered;
use serde::Deserialize;
use tokio::sync::{Notify, watch};

use crate::config::CatalogueCredentials;
use crate::error::NotifyError;

const NOTIFY_PATH: &str = "/api/internal/gateway/inventory-changed";
const SCOPE: &str = "gateway:inventory:sync";
const ATTEMPTS: u32 = 3;
const BACKOFF: Duration = Duration::from_secs(2);
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    token_endpoint: String,
}

#[derive(Deserialize)]
struct Grant {
    access_token: String,
}

/// Sends the inventory-changed notification with a fresh client-credentials token.
pub struct CatalogueNotifier {
    http: Client,
    issuer: String,
    credentials: CatalogueCredentials,
}

impl CatalogueNotifier {
    /// Builds the notifier for the identity provider and auth-service at `issuer`.
    pub fn new(issuer: &str, credentials: CatalogueCredentials) -> Result<Self, reqwest::Error> {
        let http = Client::builder()
            .timeout(TIMEOUT)
            .connect_timeout(TIMEOUT)
            .redirect(redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            issuer: issuer.to_owned(),
            credentials,
        })
    }

    /// Notifies auth-service, retrying a bounded number of times with a growing pause.
    pub async fn notify(&self) -> Result<(), NotifyError> {
        let mut attempt = 1_u32;
        loop {
            match self.attempt().await {
                Ok(()) => return Ok(()),
                Err(error) if attempt >= ATTEMPTS => return Err(error),
                Err(error) => {
                    tracing::warn!(%error, attempt, "catalogue.notify_retry");
                    tokio::time::sleep(BACKOFF * attempt).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn attempt(&self) -> Result<(), NotifyError> {
        let token = self.token().await?;
        let response = self
            .http
            .post(format!("{}{NOTIFY_PATH}", self.issuer))
            .bearer_auth(token.expose())
            .send()
            .await?;
        match response.status() {
            StatusCode::ACCEPTED => Ok(()),
            status => Err(NotifyError::Status(status)),
        }
    }

    async fn token(&self) -> Result<SecretString, NotifyError> {
        let discovery: Discovery = self
            .http
            .get(format!("{}/.well-known/openid-configuration", self.issuer))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if discovery.issuer.trim_end_matches('/') != self.issuer {
            return Err(NotifyError::IssuerMismatch);
        }
        let body = form_body(&[
            ("grant_type", "client_credentials"),
            ("scope", SCOPE),
            ("resource", &self.issuer),
        ])?;
        let grant: Grant = self
            .http
            .post(discovery.token_endpoint)
            .basic_auth(
                &self.credentials.client_id,
                Some(self.credentials.client_secret.expose()),
            )
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(SecretString::new(grant.access_token))
    }
}

fn form_body(fields: &[(&str, &str)]) -> Result<String, NotifyError> {
    let url = Url::parse_with_params("http://form.invalid/", fields)
        .map_err(|error| NotifyError::Body(error.to_string()))?;
    Ok(url.query().unwrap_or_default().to_owned())
}

/// Notifies auth-service after each signal. Signals that arrive while a notification is in
/// flight collapse into one follow-up, so a burst of changes costs at most two requests.
pub async fn run(
    notifier: CatalogueNotifier,
    signal: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            () = signal.notified() => {}
            () = triggered(&mut shutdown) => return,
        }
        tokio::select! {
            result = notifier.notify() => match result {
                Ok(()) => tracing::info!("catalogue.notified"),
                Err(error) => tracing::warn!(%error, "catalogue.notify_failed"),
            },
            () = triggered(&mut shutdown) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_request_carries_the_grant_scope_and_audience() {
        let body = form_body(&[
            ("grant_type", "client_credentials"),
            ("scope", SCOPE),
            ("resource", "https://auth.example.com"),
        ])
        .unwrap();
        assert_eq!(
            body,
            "grant_type=client_credentials&scope=gateway%3Ainventory%3Async&resource=https%3A%2F%2Fauth.example.com"
        );
    }

    #[test]
    fn the_notifier_builds_without_contacting_anyone() {
        let credentials = CatalogueCredentials {
            client_id: "reverse-relay".to_owned(),
            client_secret: SecretString::new("relay-secret".to_owned()),
        };
        assert!(CatalogueNotifier::new("https://auth.example.com", credentials).is_ok());
    }
}
