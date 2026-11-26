//! Reverse tunnel agent: exposes a local OpenAI-compatible engine to a relay without any inbound port.

pub mod auth;
pub mod benchmark;
pub mod catalog;
pub mod config;
pub mod credential;
pub mod discovery;
pub mod engine;
pub mod error;
pub mod gpu;
pub mod hardware;
pub mod host;
pub mod proxy;
pub mod status;
#[cfg(test)]
mod test_support;
pub mod tls;
pub mod tunnel;
pub mod upstream;

use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use reverse_core::status::{AgentStatus, Benchmark};
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
    let mut info = hardware::collect();
    info.hardware.host_id.clone_from(&config.host_id);
    info.hardware.hostname.clone_from(&config.hostname);
    let inflight = Arc::new(AtomicU32::new(0));
    let (benchmark_tx, benchmark_rx) = watch::channel(Benchmark::unknown());
    let (status_tx, status_rx) = watch::channel(AgentStatus::initial());
    let state = Arc::new(AgentState {
        upstream: upstream.clone(),
        models: models.clone(),
        info: info.clone(),
        status: status_rx,
        inflight: inflight.clone(),
        max_request_bytes: config.max_request_bytes,
        idle_timeout: config.idle_timeout,
    });

    let sampler = tokio::spawn(status::run(
        status::Inputs {
            upstream: upstream.clone(),
            models: models.clone(),
            benchmark: benchmark_rx,
            inflight,
            hardware: info.hardware.kind,
            max_slots: config.max_slots,
            every: config.status_poll,
        },
        status_tx,
        shutdown.clone(),
    ));
    let benchmark = config.benchmark.then(|| {
        tokio::spawn(benchmark::run(
            upstream.clone(),
            models,
            benchmark_tx,
            shutdown.clone(),
        ))
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
    for (name, task) in [("status", Some(sampler)), ("benchmark", benchmark)] {
        if let Some(task) = task
            && tokio::time::timeout(DISCOVERY_STOP_TIMEOUT, task)
                .await
                .is_err()
        {
            tracing::warn!(task = name, "task.stop_timeout");
        }
    }
}
