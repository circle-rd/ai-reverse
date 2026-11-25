//! Agent error types; their strings are for logs, never for the peer.

use std::io;
use std::time::Duration;

use http::StatusCode;
use reverse_core::secret::InvalidHeaderSecret;
use thiserror::Error;

/// A request to the local engine failed.
#[derive(Debug, Error)]
pub enum UpstreamError {
    /// The engine did not start answering in time.
    #[error("upstream did not answer within {0:?}")]
    Timeout(Duration),
    /// Connecting to or talking with the engine failed.
    #[error("upstream request failed: {0}")]
    Request(#[from] hyper_util::client::legacy::Error),
    /// The outgoing request could not be assembled.
    #[error("invalid upstream request: {0}")]
    Build(#[from] http::Error),
    /// The configured engine key cannot be sent as a header.
    #[error(transparent)]
    Credential(#[from] InvalidHeaderSecret),
    /// The answer body could not be read within its limit.
    #[error("upstream body is unreadable: {0}")]
    Body(String),
}

/// Polling the engine's model list failed.
#[derive(Debug, Error)]
pub enum DiscoveryError {
    /// The request itself failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// The poll did not finish in time.
    #[error("model list poll timed out after {0:?}")]
    Timeout(Duration),
    /// The engine answered with a non-success status.
    #[error("upstream answered {0}")]
    Status(StatusCode),
    /// The body could not be read within limits.
    #[error("model list body is unreadable: {0}")]
    Body(String),
    /// The body is not the expected JSON.
    #[error("model list is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// Looking up model metadata on the Hugging Face hub failed.
///
/// Lookups are enrichment, never a serving dependency: every failure degrades the tags of one
/// model and is logged, it never stops the tunnel.
#[derive(Debug, Error)]
pub enum CatalogError {
    /// The request itself failed.
    #[error("model metadata request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The hub answered with an unexpected status.
    #[error("model metadata answered {0}")]
    Status(reqwest::StatusCode),
    /// The metadata endpoint could not be built from the configured origin.
    #[error("invalid model metadata url: {0}")]
    Url(String),
    /// The answer exceeded its size cap.
    #[error("{0}")]
    Body(String),
    /// The answer is not the expected JSON.
    #[error("model metadata is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
}

/// The startup speed test could not produce a figure.
#[derive(Debug, Error)]
pub enum BenchmarkError {
    /// The request itself failed.
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    /// The engine refused the test request.
    #[error("the engine answered {0}")]
    Status(StatusCode),
    /// The answer stream broke or exceeded its cap.
    #[error("the answer stream failed: {0}")]
    Stream(String),
    /// The generation did not finish in time.
    #[error("the generation did not finish within {0:?}")]
    Timeout(Duration),
    /// The engine answered without generating any text.
    #[error("the engine generated no text")]
    NoTokens,
}

/// Opening or running the tunnel failed.
#[derive(Debug, Error)]
pub enum TunnelError {
    /// TCP connect to the relay failed.
    #[error("connect to relay failed: {0}")]
    Connect(#[source] io::Error),
    /// The TLS handshake with the relay failed.
    #[error("tls handshake with the relay failed: {0}")]
    Tls(#[source] io::Error),
    /// The relay host cannot be used as a TLS server name.
    #[error("the relay host is not a valid tls server name")]
    ServerName,
    /// The credential cannot be sent as a header.
    #[error(transparent)]
    Credential(InvalidHeaderSecret),
    /// A bounded step took too long.
    #[error("relay did not answer within {0:?}")]
    Timeout(Duration),
    /// The HTTP/1 handshake or request failed.
    #[error("tunnel handshake failed: {0}")]
    Handshake(#[source] hyper::Error),
    /// The relay answered something other than a protocol switch.
    #[error("relay refused the tunnel with status {0}")]
    Refused(StatusCode),
    /// The relay switched to a protocol this agent does not speak.
    #[error("relay switched to an unexpected protocol")]
    Protocol,
    /// The upgraded stream could not be obtained.
    #[error("tunnel upgrade failed: {0}")]
    Upgrade(#[source] hyper::Error),
    /// The upgrade request could not be assembled.
    #[error("invalid tunnel request: {0}")]
    Build(#[from] http::Error),
    /// The HTTP/2 connection broke.
    #[error("tunnel connection failed: {0}")]
    Serve(#[source] hyper::Error),
}
