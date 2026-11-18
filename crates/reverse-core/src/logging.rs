//! Structured JSON logging.

use tracing_subscriber::EnvFilter;

/// Installs the global JSON subscriber; `RUST_LOG` overrides the `info` default.
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_current_span(false)
        .init();
}
