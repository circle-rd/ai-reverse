//! Relay error types; their strings are for logs, never for the peer.

use std::io;
use std::net::SocketAddr;

use http::StatusCode;
use thiserror::Error;

/// A LiteLLM admin call failed.
#[derive(Debug, Error)]
pub enum LiteLlmError {
    /// The HTTP exchange failed.
    #[error("litellm request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// LiteLLM answered a non-success status.
    #[error("litellm answered {0}")]
    Status(StatusCode),
    /// A request URL could not be built.
    #[error("invalid litellm url: {0}")]
    Url(String),
}

/// The catalogue notification to auth-service failed.
#[derive(Debug, Error)]
pub enum NotifyError {
    /// The HTTP exchange failed.
    #[error("notification request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The identity provider answered for another issuer.
    #[error("identity provider issuer mismatch")]
    IssuerMismatch,
    /// The token request could not be encoded.
    #[error("invalid token request: {0}")]
    Body(String),
    /// auth-service answered something other than `202 Accepted`.
    #[error("auth-service answered {0}")]
    Status(StatusCode),
}

/// The relay could not start.
#[derive(Debug, Error)]
pub enum RelayError {
    /// A listener could not be bound.
    #[error("cannot bind {what} listener on {addr}: {source}")]
    Bind {
        /// Which listener.
        what: &'static str,
        /// The requested address.
        addr: SocketAddr,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The LiteLLM client could not be built.
    #[error("cannot build the litellm client: {0}")]
    LiteLlm(#[source] reqwest::Error),
    /// The token verifier's HTTP client could not be built.
    #[error("cannot build the identity provider client: {0}")]
    Auth(#[source] reqwest::Error),
    /// The catalogue notifier's HTTP client could not be built.
    #[error("cannot build the catalogue notifier client: {0}")]
    Catalogue(#[source] reqwest::Error),
}

/// An agent's tunnel could not be established or kept.
#[derive(Debug, Error)]
pub enum TunnelError {
    /// The upgraded stream could not be obtained.
    #[error("tunnel upgrade failed: {0}")]
    Upgrade(#[source] hyper::Error),
    /// The HTTP/2 handshake with the agent failed.
    #[error("tunnel handshake failed: {0}")]
    Handshake(#[source] hyper::Error),
    /// The HTTP/2 connection broke.
    #[error("tunnel connection failed: {0}")]
    Connection(#[source] hyper::Error),
}

/// Polling an agent's model list failed.
#[derive(Debug, Error)]
pub enum PollError {
    /// The request could not be assembled.
    #[error("invalid poll request: {0}")]
    Build(#[from] http::Error),
    /// The agent did not answer in time.
    #[error("agent did not answer the model poll in time")]
    Timeout,
    /// The HTTP/2 exchange failed.
    #[error("model poll failed: {0}")]
    Exchange(#[from] hyper::Error),
    /// The agent answered a non-success status.
    #[error("agent answered {0}")]
    Status(StatusCode),
    /// The body could not be read within limits.
    #[error("model poll body is unreadable: {0}")]
    Body(String),
    /// The body is not the expected JSON.
    #[error("model poll is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}
