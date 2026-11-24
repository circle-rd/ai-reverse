//! Polls a connected agent for the models it serves and, once, for what machine it runs on.

use std::sync::Arc;
use std::time::Duration;

use http::{Method, Request, StatusCode};
use http_body_util::{BodyExt, Limited};
use reverse_core::body::empty;
use reverse_core::ids::{ClientId, ModelName};
use reverse_core::wire::{
    AGENT_INFO_PATH, AGENT_MODELS_PATH, AgentInfo, ModelMetadata, ModelsSnapshot, TUNNEL_AUTHORITY,
};
use serde::de::DeserializeOwned;
use tokio::time::{MissedTickBehavior, interval};

use crate::error::PollError;
use crate::registry::{Registry, TunnelSender};

const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Polls until the tunnel is unusable; returning ends the tunnel.
pub async fn run(
    mut sender: TunnelSender,
    client: ClientId,
    node_id: String,
    registry: Arc<Registry>,
    every: Duration,
    request_timeout: Duration,
) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    match fetch_info(&mut sender, request_timeout).await {
        Ok(info) => registry.record_hardware(&client, &node_id, info.hardware),
        Err(PollError::Status(status)) if status == StatusCode::NOT_FOUND => {
            tracing::debug!(client_id = %client, %node_id, "agent.info_unsupported");
        }
        Err(error) => {
            tracing::warn!(client_id = %client, %node_id, %error, "agent.info_failed");
        }
    }
    let mut failures = 0_u32;
    loop {
        ticker.tick().await;
        match poll(&mut sender, request_timeout).await {
            Ok(models) => {
                failures = 0;
                registry.observe(&client, &node_id, &models);
            }
            Err(error) => {
                failures += 1;
                tracing::warn!(client_id = %client, %node_id, %error, failures, "models.poll_failed");
                if failures >= MAX_CONSECUTIVE_FAILURES || sender.is_closed() {
                    return;
                }
            }
        }
    }
}

/// Asks the agent what it runs on; agents older than this endpoint answer 404.
pub async fn fetch_info(
    sender: &mut TunnelSender,
    limit: Duration,
) -> Result<AgentInfo, PollError> {
    get_json(sender, AGENT_INFO_PATH, limit).await
}

async fn poll(
    sender: &mut TunnelSender,
    limit: Duration,
) -> Result<Vec<(ModelName, ModelMetadata)>, PollError> {
    let snapshot: ModelsSnapshot = get_json(sender, AGENT_MODELS_PATH, limit).await?;
    Ok(snapshot
        .models
        .iter()
        .filter_map(|entry| match ModelName::parse(&entry.id) {
            Ok(name) => Some((name, entry.metadata.clone())),
            Err(error) => {
                tracing::debug!(%error, "models.name_rejected");
                None
            }
        })
        .collect())
}

async fn get_json<T: DeserializeOwned>(
    sender: &mut TunnelSender,
    path: &str,
    limit: Duration,
) -> Result<T, PollError> {
    let request = Request::builder()
        .method(Method::GET)
        .uri(format!("http://{TUNNEL_AUTHORITY}{path}"))
        .body(empty())?;
    let response = tokio::time::timeout(limit, sender.send_request(request))
        .await
        .map_err(|_| PollError::Timeout)??;
    if response.status() != StatusCode::OK {
        return Err(PollError::Status(response.status()));
    }
    let bytes = tokio::time::timeout(
        limit,
        Limited::new(response.into_body(), MAX_SNAPSHOT_BYTES).collect(),
    )
    .await
    .map_err(|_| PollError::Timeout)?
    .map_err(|e| PollError::Body(e.to_string()))?
    .to_bytes();
    Ok(serde_json::from_slice(&bytes)?)
}
