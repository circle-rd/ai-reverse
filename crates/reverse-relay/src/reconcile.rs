//! Keeps LiteLLM's deployments equal to the models the connected agents serve.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reverse_core::ids::{ClientId, ModelName};
use reverse_core::shutdown::triggered;
use reverse_core::wire::ModelTag;
use tokio::sync::watch;
use tokio::time::{MissedTickBehavior, interval};

use crate::error::LiteLlmError;
use crate::litellm::{LiteLlm, Managed};
use crate::registry::Registry;

const FINAL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Capability tags to publish in the deployment's `model_info`.
    pub tags: Vec<ModelTag>,
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
/// A deployment whose tags changed is replaced: LiteLLM has no partial update for
/// `model_info`, and a stale classification is worse than one more round trip.
#[must_use]
pub fn plan(wanted: &BTreeMap<(ClientId, ModelName), Vec<ModelTag>>, actual: &[Managed]) -> Plan {
    let wanted_by_name: HashMap<String, &Vec<ModelTag>> = wanted
        .iter()
        .map(|((client, model), tags)| (format!("{client}/{model}"), tags))
        .collect();
    let mut kept = HashSet::new();
    let mut delete = Vec::new();
    for row in actual {
        let keep = match &row.reverse_name {
            Some(name) => wanted_by_name.get(name).is_some_and(|tags| {
                let wanted_tags: Vec<String> =
                    tags.iter().map(|tag| tag.as_str().to_owned()).collect();
                row.tags == wanted_tags && kept.insert(name.clone())
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
        .map(|((client, model), tags)| Desired {
            client: client.clone(),
            model: model.clone(),
            tags: tags.clone(),
        })
        .collect();
    Plan { create, delete }
}

async fn apply(
    litellm: &LiteLlm,
    wanted: &BTreeMap<(ClientId, ModelName), Vec<ModelTag>>,
) -> Result<(), LiteLlmError> {
    let actual = litellm.list_managed().await?;
    let plan = plan(wanted, &actual);
    for id in &plan.delete {
        litellm.delete(id).await?;
        tracing::info!(deployment_id = %id, "litellm.deployment_deleted");
    }
    for desired in &plan.create {
        litellm
            .create(&desired.client, &desired.model, &desired.tags)
            .await?;
        tracing::info!(
            client_id = %desired.client,
            model = %desired.model,
            tags = ?desired.tags,
            "litellm.deployment_created"
        );
    }
    Ok(())
}

/// Reconciles until shutdown, then removes everything this relay registered.
pub async fn run(
    registry: Arc<Registry>,
    litellm: LiteLlm,
    timing: Timing,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticker = interval(timing.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut applied: Option<BTreeMap<(ClientId, ModelName), Vec<ModelTag>>> = None;
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
        match apply(&litellm, &wanted).await {
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
    match tokio::time::timeout(FINAL_CLEANUP_TIMEOUT, apply(&litellm, &BTreeMap::new())).await {
        Ok(Ok(())) => tracing::info!("litellm.cleaned_up"),
        Ok(Err(error)) => tracing::warn!(%error, "litellm.cleanup_failed"),
        Err(_) => tracing::warn!("litellm.cleanup_timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(client: &str, model: &str) -> (ClientId, ModelName) {
        (
            ClientId::parse(client).unwrap(),
            ModelName::parse(model).unwrap(),
        )
    }

    fn wanted(
        entries: &[(&str, &str, &[ModelTag])],
    ) -> BTreeMap<(ClientId, ModelName), Vec<ModelTag>> {
        entries
            .iter()
            .map(|(client, model, tags)| (pair(client, model), tags.to_vec()))
            .collect()
    }

    fn row(id: &str, name: Option<&str>, tags: &[&str]) -> Managed {
        Managed {
            id: id.to_owned(),
            reverse_name: name.map(str::to_owned),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        }
    }

    #[test]
    fn creates_missing_and_deletes_unwanted() {
        let wanted = wanted(&[("tester", "new", &[ModelTag::Text])]);
        let actual = [row("1", Some("tester/old"), &["text"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(
            plan.create,
            vec![Desired {
                client: ClientId::parse("tester").unwrap(),
                model: ModelName::parse("new").unwrap(),
                tags: vec![ModelTag::Text],
            }]
        );
        assert_eq!(plan.delete, vec!["1".to_owned()]);
    }

    #[test]
    fn keeps_what_is_already_registered_with_the_same_tags() {
        let wanted = wanted(&[("tester", "m", &[ModelTag::Text])]);
        let plan = plan(&wanted, &[row("1", Some("tester/m"), &["text"])]);
        assert_eq!(plan, Plan::default());
    }

    #[test]
    fn replaces_a_deployment_whose_tags_changed() {
        let wanted = wanted(&[("tester", "m", &[ModelTag::Text, ModelTag::Tools])]);
        let actual = [row("1", Some("tester/m"), &["text"])];
        let plan = plan(&wanted, &actual);
        assert_eq!(plan.delete, vec!["1".to_owned()]);
        assert_eq!(plan.create.len(), 1);
        assert_eq!(plan.create[0].tags, vec![ModelTag::Text, ModelTag::Tools]);
    }

    #[test]
    fn deletes_duplicates_and_unrecorded_rows() {
        let wanted = wanted(&[("tester", "m", &[ModelTag::Text])]);
        let actual = [
            row("1", Some("tester/m"), &["text"]),
            row("2", Some("tester/m"), &["text"]),
            row("3", None, &[]),
        ];
        let plan = plan(&wanted, &actual);
        assert!(plan.create.is_empty());
        assert_eq!(plan.delete, vec!["2".to_owned(), "3".to_owned()]);
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
