//! Keeps LiteLLM's deployments equal to the models the connected agents serve.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reverse_core::ids::{ClientId, ModelName};
use reverse_core::shutdown::triggered;
use reverse_core::wire::{HardwareInfo, ModelMetadata};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval};

use crate::error::LiteLlmError;
use crate::litellm::{LiteLlm, Managed, Publish};
use crate::registry::{Advertised, Registry};

const FINAL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);
const APPLY_CONCURRENCY: usize = 8;

/// Timing knobs of the reconciler.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// How often wanted and applied sets are compared.
    pub interval: Duration,
    /// How often LiteLLM is re-read even without changes.
    pub resync: Duration,
    /// How long a vanished model stays registered.
    pub grace: Duration,
}

/// One deployment to register: who owns it, which model, and how to classify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Namespace that owns the model.
    pub client: ClientId,
    /// Model id as the agent announced it.
    pub model: ModelName,
    /// Model metadata to publish in the deployment's `model_info`.
    pub metadata: ModelMetadata,
    /// Hardware of the nodes serving the model, published in the deployment's `model_info`.
    pub hardware: Vec<HardwareInfo>,
}

/// What to change in LiteLLM.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Deployments to register.
    pub create: Vec<Desired>,
    /// Deployment ids to remove: unwanted, duplicated, unrecorded or with changed tags.
    pub delete: Vec<String>,
}

/// Diffs the wanted pairs against what LiteLLM holds.
///
/// A deployment whose tags, owner, capability flags or hardware changed is replaced: LiteLLM
/// has no partial update for `model_info`, and a stale classification is worse than one more
/// round trip.
#[must_use]
pub fn plan(wanted: &BTreeMap<(ClientId, ModelName), Advertised>, actual: &[Managed]) -> Plan {
    let wanted_by_name: HashMap<String, (&ClientId, &Advertised)> = wanted
        .iter()
        .map(|((client, model), advertised)| (format!("{client}/{model}"), (client, advertised)))
        .collect();
    let mut kept = HashSet::new();
    let mut delete = Vec::new();
    for row in actual {
        let keep = match &row.reverse_name {
            Some(name) => wanted_by_name
                .get(name)
                .is_some_and(|(client, advertised)| {
                    let metadata = &advertised.metadata;
                    let wanted_tags: Vec<String> = metadata
                        .tags
                        .iter()
                        .map(|tag| tag.as_str().to_owned())
                        .collect();
                    let publish = Publish::from_tags(&metadata.tags);
                    row.tags == wanted_tags
                        && row.hub_owner.as_deref() == metadata.hub_owner.as_deref()
                        && row.hub_avatar_url.as_deref() == metadata.hub_avatar_url.as_deref()
                        && row.author_safety_claim == metadata.author_safety_claim
                        && row.created_by.as_deref() == Some(client.as_str())
                        && row.mode.as_deref() == publish.mode
                        && row.supports == publish.supports
                        && row.hardware == advertised.hardware
                        && kept.insert(name.clone())
                }),
            None => false,
        };
        if !keep {
            delete.push(row.id.clone());
        }
    }
    let create = wanted
        .iter()
        .filter(|((client, model), _)| !kept.contains(&format!("{client}/{model}")))
        .map(|((client, model), advertised)| Desired {
            client: client.clone(),
            model: model.clone(),
            metadata: advertised.metadata.clone(),
            hardware: advertised.hardware.clone(),
        })
        .collect();
    Plan { create, delete }
}

/// Applies the wanted set. Signals `catalogue` once the mutations are over, whatever their
/// outcome: auth-service re-reads LiteLLM itself, so a partial change still needs announcing.
async fn apply(
    litellm: &LiteLlm,
    wanted: &BTreeMap<(ClientId, ModelName), Advertised>,
    catalogue: &Notify,
) -> Result<(), LiteLlmError> {
    let actual = litellm.list_managed().await?;
    let plan = plan(wanted, &actual);
    let changes = !plan.create.is_empty() || !plan.delete.is_empty();
    let outcome = mutate(litellm, plan).await;
    if changes {
        catalogue.notify_one();
    }
    outcome
}

async fn mutate(litellm: &LiteLlm, plan: Plan) -> Result<(), LiteLlmError> {
    // Deleting before creating keeps an alias from being registered twice.
    let mut tasks: JoinSet<(String, Result<(), LiteLlmError>)> = JoinSet::new();
    let mut pending = plan.delete.into_iter();
    let mut failure: Option<LiteLlmError> = None;
    loop {
        while tasks.len() < APPLY_CONCURRENCY {
            let Some(id) = pending.next() else { break };
            let litellm = litellm.clone();
            tasks.spawn(async move {
                let result = litellm.delete(&id).await;
                (id, result)
            });
        }
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        match joined {
            Ok((id, Ok(()))) => tracing::info!(deployment_id = %id, "litellm.deployment_deleted"),
            Ok((id, Err(error))) => {
                tracing::warn!(deployment_id = %id, %error, "litellm.delete_failed");
                failure.get_or_insert(error);
            }
            Err(error) => tracing::warn!(%error, "litellm.delete_task_failed"),
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }

    let mut tasks: JoinSet<(ClientId, ModelName, Result<(), LiteLlmError>)> = JoinSet::new();
    let mut pending = plan.create.into_iter();
    loop {
        while tasks.len() < APPLY_CONCURRENCY {
            let Some(desired) = pending.next() else { break };
            let litellm = litellm.clone();
            tasks.spawn(async move {
                let result = litellm
                    .create(
                        &desired.client,
                        &desired.model,
                        &desired.metadata,
                        &desired.hardware,
                    )
                    .await;
                (desired.client, desired.model, result)
            });
        }
        let Some(joined) = tasks.join_next().await else {
            break;
        };
        match joined {
            Ok((client, model, Ok(()))) => {
                tracing::info!(client_id = %client, model = %model, "litellm.deployment_created");
            }
            Ok((client, model, Err(error))) => {
                tracing::warn!(client_id = %client, model = %model, %error, "litellm.create_failed");
                failure.get_or_insert(error);
            }
            Err(error) => tracing::warn!(%error, "litellm.create_task_failed"),
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Reconciles until shutdown, then removes everything this relay registered.
pub async fn run(
    registry: Arc<Registry>,
    litellm: LiteLlm,
    timing: Timing,
    catalogue: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(timing.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut applied: Option<BTreeMap<(ClientId, ModelName), Advertised>> = None;
    let mut last_sync: Option<Instant> = None;
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            () = registry.changed() => {}
            () = triggered(&mut shutdown) => break,
        }
        let wanted = registry.wanted(timing.grace);
        let resync_due = last_sync.is_none_or(|at| at.elapsed() >= timing.resync);
        if applied.as_ref() == Some(&wanted) && !resync_due {
            continue;
        }
        match apply(&litellm, &wanted, &catalogue).await {
            Ok(()) => {
                applied = Some(wanted);
                last_sync = Some(Instant::now());
            }
            Err(error) => {
                // Keep `applied` as is so the next tick retries.
                tracing::warn!(%error, "litellm.sync_failed");
                applied = None;
            }
        }
    }
    match tokio::time::timeout(
        FINAL_CLEANUP_TIMEOUT,
        apply(&litellm, &BTreeMap::new(), &catalogue),
    )
    .await
    {
        Ok(Ok(())) => tracing::info!("litellm.cleaned_up"),
        Ok(Err(error)) => tracing::warn!(%error, "litellm.cleanup_failed"),
        Err(_) => tracing::warn!("litellm.cleanup_timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::litellm::Supports;
    use reverse_core::wire::{AuthorSafetyClaim, HardwareKind, ModelTag};

    fn pair(client: &str, model: &str) -> (ClientId, ModelName) {
        (
            ClientId::parse(client).unwrap(),
            ModelName::parse(model).unwrap(),
        )
    }

    fn wanted(
        entries: &[(&str, &str, &[ModelTag])],
    ) -> BTreeMap<(ClientId, ModelName), Advertised> {
        entries
            .iter()
            .map(|(client, model, tags)| {
                (
                    pair(client, model),
                    Advertised {
                        metadata: ModelMetadata {
                            tags: tags.to_vec(),
                            ..ModelMetadata::default()
                        },
                        hardware: Vec::new(),
                    },
                )
            })
            .collect()
    }

    fn gb10() -> HardwareInfo {
        HardwareInfo {
            kind: HardwareKind::DgxSpark,
            product: Some("NVIDIA DGX Spark".to_owned()),
            vendor: None,
            gpus: vec!["NVIDIA GB10".to_owned()],
            memory_mb: 122_880,
            cpu_arch: "aarch64".to_owned(),
            cpu_cores: 20,
        }
    }

    fn row(id: &str, name: Option<&str>, tags: &[&str]) -> Managed {
        let parsed: Vec<ModelTag> = tags.iter().filter_map(|tag| ModelTag::parse(tag)).collect();
        let publish = Publish::from_tags(&parsed);
        Managed {
            id: id.to_owned(),
            reverse_name: name.map(str::to_owned),
            created_by: name
                .and_then(|name| name.split_once('/'))
                .map(|(owner, _)| owner.to_owned()),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            hub_owner: None,
            hub_avatar_url: None,
            author_safety_claim: None,
            mode: publish.mode.map(str::to_owned),
            supports: publish.supports,
            hardware: Vec::new(),
        }
    }

    fn legacy_row(id: &str, name: &str, tags: &[&str]) -> Managed {
        Managed {
            created_by: None,
            mode: None,
            supports: Supports::default(),
            ..row(id, Some(name), tags)
        }
    }

    #[test]
    fn creates_missing_and_deletes_unwanted() {
        let wanted = wanted(&[("u-tester", "new", &[ModelTag::Text])]);
        let actual = [row("1", Some("u-tester/old"), &["text"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(
            plan.create,
            vec![Desired {
                client: ClientId::parse("u-tester").unwrap(),
                model: ModelName::parse("new").unwrap(),
                metadata: ModelMetadata {
                    tags: vec![ModelTag::Text],
                    ..ModelMetadata::default()
                },
                hardware: Vec::new(),
            }]
        );
        assert_eq!(plan.delete, vec!["1".to_owned()]);
    }

    #[test]
    fn keeps_what_is_already_registered_with_the_same_tags() {
        let wanted = wanted(&[("u-tester", "m", &[ModelTag::Text])]);
        let plan = plan(&wanted, &[row("1", Some("u-tester/m"), &["text"])]);
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn replaces_a_deployment_whose_tags_changed() {
        let wanted = wanted(&[("u-tester", "m", &[ModelTag::Text, ModelTag::Tools])]);
        let actual = [row("1", Some("u-tester/m"), &["text"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(
            plan.create[0].metadata.tags,
            vec![ModelTag::Text, ModelTag::Tools]
        );
    }

    #[test]
    fn replaces_a_deployment_when_hub_display_metadata_changes() {
        let client = ClientId::parse("u-tester").unwrap();
        let model = ModelName::parse("Org/model").unwrap();
        let metadata = ModelMetadata {
            tags: vec![ModelTag::Text],
            hub_owner: Some("Org".to_owned()),
            hub_avatar_url: Some("https://cdn-avatars.huggingface.co/avatar.png".to_owned()),
            author_safety_claim: Some(AuthorSafetyClaim::Abliterated),
        };
        let wanted = BTreeMap::from([(
            (client.clone(), model.clone()),
            Advertised {
                metadata: metadata.clone(),
                hardware: Vec::new(),
            },
        )]);
        let mut actual = row("1", Some("u-tester/Org/model"), &["text"]);
        actual.hub_owner = metadata.hub_owner.clone();
        actual.hub_avatar_url = metadata.hub_avatar_url.clone();
        actual.author_safety_claim = metadata.author_safety_claim;
        assert_eq!(plan(&wanted, &[actual.clone()]), Plan::default());

        actual.author_safety_claim = None;
        let plan = plan(&wanted, &[actual]);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create[0].metadata, metadata);
    }

    #[test]
    fn keeps_a_deployment_whose_hardware_is_unchanged() {
        let mut wanted = wanted(&[("u-tester", "m", &[ModelTag::Text])]);
        for advertised in wanted.values_mut() {
            advertised.hardware = vec![gb10()];
        }
        let mut actual = row("1", Some("u-tester/m"), &["text"]);
        actual.hardware = vec![gb10()];
        assert_eq!(plan(&wanted, &[actual]), Plan::default());
    }

    #[test]
    fn replaces_a_deployment_whose_hardware_changed() {
        let mut wanted = wanted(&[("u-tester", "m", &[ModelTag::Text])]);
        for advertised in wanted.values_mut() {
            advertised.hardware = vec![gb10()];
        }
        let plan = plan(&wanted, &[row("1", Some("u-tester/m"), &["text"])]);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create[0].hardware, vec![gb10()]);
    }

    #[test]
    fn replaces_a_deployment_missing_the_owner_or_capability_flags() {
        let wanted = wanted(&[("u-tester", "m", &[ModelTag::Text, ModelTag::Tools])]);
        let actual = [legacy_row("1", "u-tester/m", &["text", "tools"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create.len(), 1);
    }

    #[test]
    fn deletes_duplicates_and_unrecorded_rows() {
        let wanted = wanted(&[("u-tester", "m", &[ModelTag::Text])]);
        let actual = [
            row("1", Some("u-tester/m"), &["text"]),
            row("2", Some("u-tester/m"), &["text"]),
            row("3", None, &[]),
        ];
        let plan = plan(&wanted, &actual);
        assert!(plan.create.is_empty());
        assert_eq!(plan.delete, vec!["2".to_owned(), "3".to_owned()]);
    }

    #[test]
    fn replaces_deployments_named_after_the_plain_username() {
        let wanted = wanted(&[("u-bmz", "m", &[ModelTag::Text])]);
        let actual = [row("1", Some("bmz/m"), &["text"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(plan.create[0].client.as_str(), "u-bmz");
    }

    #[test]
    fn an_empty_wanted_set_clears_everything() {
        let actual = [
            row("1", Some("a/b"), &["text"]),
            row("2", Some("c/d"), &["text"]),
        ];
        let plan = plan(&BTreeMap::new(), &actual);
        assert_eq!(plan.delete.len(), 2);
    }
}
