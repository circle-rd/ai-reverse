//! Reverse tunnel agent: exposes a local OpenAI-compatible engine to a relay without any inbound port.

pub mod auth;
pub mod catalog;
pub mod config;
pub mod credential;
pub mod discovery;
pub mod error;
pub mod hardware;
pub mod proxy;
pub mod tls;
pub mod tunnel;
pub mod upstream;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

pub use config::Config;

use crate::catalog::Catalog;
use crate::proxy::AgentState;
use crate::upstream::Upstream;

const DISCOVERY_STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the agent until `shutdown` flips to `true`.
pub async fn run(config: Config, shutdown: watch::Receiver<bool>) {
    let config = Arc::new(config);
    let (snapshots, models) = discovery::channel();
    let upstream = Arc::new(Upstream::new(&config));
    let state = Arc::new(AgentState {
        upstream: upstream.clone(),
        models,
        info: hardware::collect(),
        max_request_bytes: config.max_request_bytes,
        idle_timeout: config.idle_timeout,
    });

    let catalog = if config.hf_enabled {
        match Catalog::new(&config) {
            Ok(catalog) => {
                tracing::info!(
                    hub = %config.hf_url.origin(),
                    "catalog.enabled; model ids are sent to the hub, AGENT_HF_METADATA=false keeps them local"
                );
                Some(Arc::new(catalog))
            }
            Err(error) => {
                tracing::error!(%error, "catalog.init_failed");
                None
            }
        }
    } else {
        tracing::debug!("catalog.disabled");
        None
    };
    let declared = discovery::Declared {
        extras: config.extra_models.clone(),
        tags: config.model_tags.clone(),
    };

    let discovery = tokio::spawn(discovery::run(
        upstream,
        config.models_poll,
        declared,
        catalog,
        config.hf_timeout,
        snapshots,
        shutdown.clone(),
    ));

    tunnel::run(config, state, shutdown).await;

    if tokio::time::timeout(DISCOVERY_STOP_TIMEOUT, discovery)
        .await
        .is_err()
    {
        tracing::warn!("discovery.stop_timeout");
    }
}
