//! Keeps a validated snapshot of the models the local engine serves.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use http::{HeaderMap, Method};
use http_body_util::{BodyExt, Limited};
use reverse_core::body::empty;
use reverse_core::ids::ModelName;
use reverse_core::shutdown::triggered;
use reverse_core::wire::{ModelEntry, ModelTag, ModelsSnapshot};
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior, interval};

use crate::catalog::Catalog;
use crate::error::DiscoveryError;
use crate::upstream::Upstream;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_MODEL_LIST_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
struct ModelList {
    data: Vec<EngineModel>,
}

#[derive(Deserialize)]
struct EngineModel {
    id: String,
}

/// What the operator declared for this agent.
#[derive(Debug, Clone, Default)]
pub struct Declared {
    /// Models announced although the engine's `/v1/models` omits them.
    pub extras: Vec<ModelName>,
    /// Tags that override anything resolved from the hub.
    pub tags: BTreeMap<ModelName, Vec<ModelTag>>,
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
    declared: Declared,
    catalog: Option<Arc<Catalog>>,
    budget: Duration,
    snapshots: watch::Sender<ModelsSnapshot>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut resolved: BTreeMap<ModelName, Vec<ModelTag>> = BTreeMap::new();
    let mut unknown = HashSet::new();
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = triggered(&mut shutdown) => return,
        }
        let outcome = match tokio::time::timeout(FETCH_TIMEOUT, fetch(&upstream)).await {
            Ok(result) => result,
            Err(_) => Err(DiscoveryError::Timeout(FETCH_TIMEOUT)),
        };
        if let Ok(ids) = &outcome {
            let fresh = resolve_pass(
                ids,
                &declared,
                catalog.as_deref(),
                &resolved,
                &mut unknown,
                budget,
            )
            .await;
            if !fresh.is_empty() {
                resolved.extend(fresh);
            }
        }
        publish(&snapshots, outcome, &declared, &resolved);
    }
}

/// Tags the announced models the hub has not classified yet.
///
/// One pass is bounded by `budget` so a slow or unreachable hub can never postpone snapshot
/// publication by more than one lookup past it; unresolved models are retried by later
/// cycles while the negative cache keeps them cheap.
async fn resolve_pass(
    ids: &[String],
    declared: &Declared,
    catalog: Option<&Catalog>,
    settled: &BTreeMap<ModelName, Vec<ModelTag>>,
    unknown: &mut HashSet<ModelName>,
    budget: Duration,
) -> BTreeMap<ModelName, Vec<ModelTag>> {
    let mut fresh = BTreeMap::new();
    let mut pending = BTreeSet::new();
    pending.extend(ids.iter().filter_map(|id| ModelName::parse(id).ok()));
    pending.extend(declared.extras.iter().cloned());
    let deadline = Instant::now() + budget;
    for name in pending {
        if declared.tags.contains_key(&name)
            || settled.contains_key(&name)
            || fresh.contains_key(&name)
        {
            continue;
        }
        if Instant::now() >= deadline {
            tracing::debug!("catalog.pass_budget_exhausted");
            break;
        }
        match catalog {
            Some(catalog) => match catalog.resolve(&name).await {
                Some(tags) => {
                    fresh.insert(name, tags);
                }
                None => {
                    if unknown.insert(name.clone()) {
                        tracing::warn!(
                            model = %name,
                            "models.tags_unknown; declare AGENT_MODEL_TAGS to classify it"
                        );
                    }
                }
            },
            None => {
                if unknown.insert(name.clone()) {
                    tracing::debug!(model = %name, "models.tags_disabled");
                }
            }
        }
    }
    fresh
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
    declared: &Declared,
    resolved: &BTreeMap<ModelName, Vec<ModelTag>>,
) {
    let (available, models, rejected) = match outcome {
        Ok(ids) => {
            let (models, rejected) = assemble(ids, declared, resolved);
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

/// Merges discovered ids with configured extras, validates them and attaches tags.
///
/// Precedence is operator declaration, then hub metadata; a model with neither carries no
/// tags rather than a guess.
fn assemble(
    ids: Vec<String>,
    declared: &Declared,
    resolved: &BTreeMap<ModelName, Vec<ModelTag>>,
) -> (Vec<ModelEntry>, Vec<String>) {
    let mut announced = BTreeMap::<ModelName, ()>::new();
    let mut rejected = Vec::new();
    for id in ids {
        match ModelName::parse(&id) {
            Ok(name) => {
                announced.insert(name, ());
            }
            Err(_) => rejected.push(id),
        }
    }
    for extra in &declared.extras {
        announced.insert(extra.clone(), ());
    }
    let models = announced
        .into_keys()
        .map(|name| {
            let mut tags = declared
                .tags
                .get(&name)
                .or_else(|| resolved.get(&name))
                .cloned()
                .unwrap_or_default();
            tags.sort();
            tags.dedup();
            ModelEntry {
                id: name.as_str().to_owned(),
                tags,
            }
        })
        .collect();
    (models, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str) -> ModelName {
        ModelName::parse(name).unwrap()
    }

    #[test]
    fn assemble_sorts_dedups_and_rejects() {
        let (models, bad) = assemble(
            vec!["b".into(), "a".into(), "b".into(), "/models/x.gguf".into()],
            &Declared::default(),
            &BTreeMap::new(),
        );
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a", "b"]);
        assert_eq!(bad, ["/models/x.gguf"]);
        assert!(models[0].tags.is_empty(), "unknown models carry no guess");
    }

    #[test]
    fn operator_tags_win_over_hub_metadata_and_extras_are_merged() {
        let declared = Declared {
            extras: vec![model("perplexity-ai/pplx-embed-v1-0.6b")],
            tags: BTreeMap::from([(
                model("qwen3.8-flash-next"),
                vec![ModelTag::Text, ModelTag::Tools],
            )]),
        };
        let resolved = BTreeMap::from([
            (
                model("perplexity-ai/pplx-embed-v1-0.6b"),
                vec![ModelTag::Embedding],
            ),
            (model("qwen3.8-flash-next"), vec![ModelTag::Text]),
        ]);
        let (models, rejected) = assemble(vec!["qwen3.8-flash-next".into()], &declared, &resolved);
        assert!(rejected.is_empty());
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "perplexity-ai/pplx-embed-v1-0.6b");
        assert_eq!(models[0].tags, vec![ModelTag::Embedding]);
        assert_eq!(models[1].id, "qwen3.8-flash-next");
        assert_eq!(models[1].tags, vec![ModelTag::Text, ModelTag::Tools]);
    }

    #[test]
    fn publish_bumps_revision_only_on_change() {
        let (tx, rx) = channel();
        let declared = Declared::default();
        let resolved = BTreeMap::new();
        publish(&tx, Ok(vec!["m".into()]), &declared, &resolved);
        publish(&tx, Ok(vec!["m".into()]), &declared, &resolved);
        assert_eq!(rx.borrow().revision, 1);
        assert!(rx.borrow().engine_available);
        publish(
            &tx,
            Err(DiscoveryError::Status(http::StatusCode::BAD_GATEWAY)),
            &declared,
            &resolved,
        );
        assert_eq!(rx.borrow().revision, 2);
        assert!(!rx.borrow().engine_available);
        assert!(rx.borrow().models.is_empty());
    }

    #[tokio::test]
    async fn a_pass_cut_by_the_budget_makes_no_lookup() {
        let map = std::collections::HashMap::from([
            (
                "AGENT_RELAY_URL".to_owned(),
                "http://127.0.0.1:8443".to_owned(),
            ),
            (
                "AGENT_UPSTREAM_URL".to_owned(),
                "http://127.0.0.1:30001".to_owned(),
            ),
            (
                "AGENT_AUTH_ISSUER".to_owned(),
                "http://127.0.0.1:9000".to_owned(),
            ),
            (
                "AGENT_AUTH_CLIENT_ID".to_owned(),
                "reverse-agent".to_owned(),
            ),
            ("AGENT_HF_URL".to_owned(), "http://127.0.0.1:1".to_owned()),
        ]);
        let config = crate::config::Config::from_env(&reverse_core::config::Env::new(move |key| {
            map.get(key).cloned()
        }))
        .unwrap();
        let catalog = crate::catalog::Catalog::new(&config).unwrap();
        let mut unknown = HashSet::new();

        let fresh = resolve_pass(
            &["m".to_owned()],
            &Declared::default(),
            Some(&catalog),
            &BTreeMap::new(),
            &mut unknown,
            Duration::ZERO,
        )
        .await;
        assert!(fresh.is_empty());
    }

    #[tokio::test]
    async fn a_pass_only_resolves_what_the_operator_did_not_declare() {
        let declared = Declared {
            extras: Vec::new(),
            tags: BTreeMap::from([(model("m"), vec![ModelTag::Text])]),
        };
        let mut unknown = HashSet::new();

        let fresh = resolve_pass(
            &["m".to_owned()],
            &declared,
            None,
            &BTreeMap::new(),
            &mut unknown,
            Duration::ZERO,
        )
        .await;
        assert!(fresh.is_empty());
        assert!(unknown.is_empty());
    }
}
