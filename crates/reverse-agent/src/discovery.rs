//! Keeps a validated snapshot of the models the local engine serves.

use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, Method};
use http_body_util::{BodyExt, Limited};
use reverse_core::body::empty;
use reverse_core::ids::ModelName;
use reverse_core::shutdown::triggered;
use reverse_core::wire::ModelsSnapshot;
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};

use crate::error::DiscoveryError;
use crate::upstream::Upstream;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MODEL_LIST_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ModelEntry>,
}

#[derive(Deserialize)]
struct ModelEntry {
    id: String,
}

/// The snapshot channel, starting before the first poll.
#[must_use]
pub fn channel() -> (
    watch::Sender<ModelsSnapshot>,
    watch::Receiver<ModelsSnapshot>,
) {
    watch::channel(ModelsSnapshot::initial())
}

/// Polls the engine until shutdown, publishing changes.
pub async fn run(
    upstream: Arc<Upstream>,
    every: Duration,
    snapshots: watch::Sender<ModelsSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = triggered(&mut shutdown) => return,
        }
        let outcome = match tokio::time::timeout(FETCH_TIMEOUT, fetch(&upstream)).await {
            Ok(result) => result,
            Err(_) => Err(DiscoveryError::Timeout(FETCH_TIMEOUT)),
        };
        publish(&snapshots, outcome);
    }
}

async fn fetch(upstream: &Upstream) -> Result<Vec<String>, DiscoveryError> {
    let response = upstream
        .send(Method::GET, "/v1/models", HeaderMap::new(), empty())
        .await?;
    if !response.status().is_success() {
        return Err(DiscoveryError::Status(response.status()));
    }
    let bytes = Limited::new(response.into_body(), MAX_MODEL_LIST_BYTES)
        .collect()
        .await
        .map_err(|e| DiscoveryError::Body(e.to_string()))?
        .to_bytes();
    let list: ModelList = serde_json::from_slice(&bytes)?;
    Ok(list.data.into_iter().map(|entry| entry.id).collect())
}

fn publish(
    snapshots: &watch::Sender<ModelsSnapshot>,
    outcome: Result<Vec<String>, DiscoveryError>,
) {
    let (available, models, rejected) = match outcome {
        Ok(ids) => {
            let (models, rejected) = validate(ids);
            (true, models, rejected)
        }
        Err(error) => {
            if snapshots.borrow().engine_available {
                tracing::warn!(%error, "engine.unavailable");
            }
            (false, Vec::new(), Vec::new())
        }
    };
    let changed = snapshots.send_if_modified(|snapshot| {
        if snapshot.engine_available == available && snapshot.models == models {
            return false;
        }
        snapshot.engine_available = available;
        snapshot.models = models.clone();
        snapshot.revision += 1;
        true
    });
    if changed {
        tracing::info!(models = ?models, rejected = ?rejected, available, "models.changed");
    }
}

fn validate(ids: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut accepted = Vec::new();
    let mut rejected = Vec::new();
    for id in ids {
        match ModelName::parse(&id) {
            Ok(name) => accepted.push(name.as_str().to_owned()),
            Err(_) => rejected.push(id),
        }
    }
    accepted.sort();
    accepted.dedup();
    (accepted, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_sorts_dedups_and_rejects() {
        let (ok, bad) = validate(vec![
            "b".into(),
            "a".into(),
            "b".into(),
            "/models/x.gguf".into(),
        ]);
        assert_eq!(ok, ["a", "b"]);
        assert_eq!(bad, ["/models/x.gguf"]);
    }

    #[test]
    fn publish_bumps_revision_only_on_change() {
        let (tx, rx) = channel();
        publish(&tx, Ok(vec!["m".into()]));
        publish(&tx, Ok(vec!["m".into()]));
        assert_eq!(rx.borrow().revision, 1);
        assert!(rx.borrow().engine_available);
        publish(
            &tx,
            Err(DiscoveryError::Status(http::StatusCode::BAD_GATEWAY)),
        );
        assert_eq!(rx.borrow().revision, 2);
        assert!(!rx.borrow().engine_available);
        assert!(rx.borrow().models.is_empty());
    }
}
