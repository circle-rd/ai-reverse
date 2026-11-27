//! Capability tags the inference server declares about the models it serves.
//!
//! The server is recognised by the endpoint it answers, in a fixed order. A server that answers
//! none of them (vLLM exposes no capability) contributes no tag, and the hub or the operator
//! decides. Nothing is inferred from a model name. Answers are memoised for as long as the
//! announced ids stay the same, so a steady engine costs no probe at all.

use std::collections::BTreeMap;
use std::time::Duration;

use http::StatusCode;
use reverse_core::ids::ModelName;
use reverse_core::wire::ModelTag;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::upstream::Upstream;

const PROBE_BUDGET: Duration = Duration::from_secs(2);
const MAX_PROBE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    /// Ollama: `/api/tags` lists its models and `/api/show` describes each one.
    Ollama,
    /// SGLang: `/model_info`, or `/get_model_info` before the rename.
    Sglang,
    /// llama.cpp `llama-server`: `/props`.
    LlamaCpp,
    /// Any other server, vLLM included.
    Other,
}

/// What earlier passes learned, kept while the announced ids stay the same.
#[derive(Debug, Default)]
pub struct Memo {
    ids: Vec<String>,
    family: Option<Family>,
    /// SGLang and llama.cpp serve one model, whose facts apply to every announced id.
    server: Option<Vec<ModelTag>>,
    /// Ollama describes each model on its own.
    per_model: BTreeMap<String, Vec<ModelTag>>,
}

impl Memo {
    /// Forgets everything: the engine may be another server now.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Reads the tags of each announced id the engine can describe, within `budget`.
///
/// A probe cut by the budget is not memoised, so the next pass asks again.
pub async fn read(
    upstream: &Upstream,
    ids: &[String],
    memo: &mut Memo,
    budget: Duration,
) -> BTreeMap<ModelName, Vec<ModelTag>> {
    let mut sorted = ids.to_vec();
    sorted.sort();
    if sorted != memo.ids {
        memo.reset();
        memo.ids = sorted;
    }
    let deadline = Instant::now() + budget;
    if memo.family.is_none() {
        memo.family = detect(upstream, deadline).await;
        if let Some(family) = memo.family {
            tracing::info!(family = ?family, "engine.family_detected");
        }
    }
    let names: Vec<ModelName> = ids
        .iter()
        .filter_map(|id| ModelName::parse(id).ok())
        .collect();
    let mut found = BTreeMap::new();
    match memo.family {
        Some(Family::Ollama) => {
            for name in names {
                if !memo.per_model.contains_key(name.as_str())
                    && let Some(tags) = ollama_tags(upstream, &name, deadline).await
                {
                    memo.per_model.insert(name.as_str().to_owned(), tags);
                }
                if let Some(tags) = memo.per_model.get(name.as_str()) {
                    found.insert(name, tags.clone());
                }
            }
        }
        Some(family @ (Family::Sglang | Family::LlamaCpp)) => {
            if memo.server.is_none() {
                memo.server = server_tags(upstream, family, deadline).await;
            }
            if let Some(tags) = &memo.server {
                for name in names {
                    found.insert(name, tags.clone());
                }
            }
        }
        Some(Family::Other) | None => {}
    }
    found
}

enum Answer {
    Json(Value),
    /// The server answered 404: the endpoint does not exist, or the model is unknown.
    Absent,
}

/// Asks one endpoint, bounded by what remains of the pass. `None` when no determinate answer
/// came back: timeout, transport failure, other status or a body that is not JSON.
async fn ask(
    upstream: &Upstream,
    path: &str,
    body: Option<Value>,
    deadline: Instant,
) -> Option<Answer> {
    let budget = deadline.saturating_duration_since(Instant::now());
    if budget.is_zero() {
        return None;
    }
    let budget = budget.min(PROBE_BUDGET);
    let result = match body {
        Some(body) => {
            let bytes = serde_json::to_vec(&body).ok()?;
            upstream
                .post_json_text(path, bytes, MAX_PROBE_BYTES, budget)
                .await
        }
        None => upstream.get_text(path, MAX_PROBE_BYTES, budget).await,
    };
    match result {
        Ok((StatusCode::OK, text)) => serde_json::from_str(&text).ok().map(Answer::Json),
        Ok((StatusCode::NOT_FOUND, _)) => Some(Answer::Absent),
        Ok((status, _)) => {
            tracing::debug!(path, status = status.as_u16(), "engine.probe_unexpected");
            None
        }
        Err(error) => {
            tracing::debug!(path, %error, "engine.probe_failed");
            None
        }
    }
}

async fn detect(upstream: &Upstream, deadline: Instant) -> Option<Family> {
    if matches!(
        ask(upstream, "/api/tags", None, deadline).await?,
        Answer::Json(value) if value.get("models").is_some()
    ) {
        return Some(Family::Ollama);
    }
    for path in ["/model_info", "/get_model_info"] {
        if matches!(
            ask(upstream, path, None, deadline).await?,
            Answer::Json(value) if value.get("is_generation").is_some()
        ) {
            return Some(Family::Sglang);
        }
    }
    match ask(upstream, "/props", None, deadline).await? {
        Answer::Json(value)
            if value.get("total_slots").is_some() || value.get("chat_template_caps").is_some() =>
        {
            Some(Family::LlamaCpp)
        }
        _ => Some(Family::Other),
    }
}

async fn ollama_tags(
    upstream: &Upstream,
    name: &ModelName,
    deadline: Instant,
) -> Option<Vec<ModelTag>> {
    match ask(
        upstream,
        "/api/show",
        Some(json!({ "model": name.as_str() })),
        deadline,
    )
    .await?
    {
        Answer::Json(value) => Some(finish(hints_of(value.get("capabilities")))),
        Answer::Absent => None,
    }
}

async fn server_tags(
    upstream: &Upstream,
    family: Family,
    deadline: Instant,
) -> Option<Vec<ModelTag>> {
    let value = match family {
        Family::Sglang => match ask(upstream, "/model_info", None, deadline).await? {
            Answer::Json(value) => value,
            Answer::Absent => match ask(upstream, "/get_model_info", None, deadline).await? {
                Answer::Json(value) => value,
                Answer::Absent => return None,
            },
        },
        Family::LlamaCpp => match ask(upstream, "/props", None, deadline).await? {
            Answer::Json(value) => value,
            Answer::Absent => return None,
        },
        Family::Ollama | Family::Other => return None,
    };
    Some(finish(match family {
        Family::Sglang => sglang_tags(&value),
        _ => llama_tags(&value),
    }))
}

/// `is_generation` tells a chat model from an embedding model; the understanding flags add the
/// input modalities.
fn sglang_tags(value: &Value) -> Vec<ModelTag> {
    let mut tags = Vec::new();
    match value.get("is_generation").and_then(Value::as_bool) {
        Some(true) => tags.push(ModelTag::Text),
        Some(false) => tags.push(ModelTag::Embedding),
        None => {}
    }
    if flag(value, "has_image_understanding") {
        tags.push(ModelTag::Vision);
    }
    if flag(value, "has_audio_understanding") {
        tags.push(ModelTag::Speech);
    }
    tags
}

/// A chat template makes a chat model; `modalities` and the template's tool support add the rest.
fn llama_tags(value: &Value) -> Vec<ModelTag> {
    let mut tags = Vec::new();
    if let Some(modalities) = value.get("modalities").and_then(Value::as_object) {
        tags.extend(
            modalities
                .iter()
                .filter(|(_, enabled)| enabled.as_bool() == Some(true))
                .filter_map(|(key, _)| ModelTag::from_hint(key)),
        );
    }
    if value
        .get("chat_template")
        .and_then(Value::as_str)
        .is_some_and(|template| !template.trim().is_empty())
    {
        tags.push(ModelTag::Text);
    }
    if let Some(caps) = value.get("chat_template_caps")
        && (flag(caps, "supports_tools") || flag(caps, "supports_tool_calls"))
    {
        tags.push(ModelTag::Tools);
    }
    tags
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}

/// Maps the capability strings of a server's answer through the shared hint vocabulary;
/// strings it does not know, such as `thinking`, are ignored.
fn hints_of(value: Option<&Value>) -> Vec<ModelTag> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(ModelTag::from_hint)
        .collect()
}

fn finish(mut tags: Vec<ModelTag>) -> Vec<ModelTag> {
    tags.sort();
    tags.dedup();
    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Route, upstream_serving};

    const OLLAMA: &[Route] = &[
        ("/api/tags", 200, r#"{"models":[]}"#),
        (
            "/api/show",
            200,
            r#"{"capabilities":["completion","tools","vision","thinking"]}"#,
        ),
    ];

    const SGLANG: &[Route] = &[(
        "/model_info",
        200,
        r#"{"model_path":"m","is_generation":true,"has_image_understanding":true,"has_audio_understanding":false}"#,
    )];

    const SGLANG_EMBEDDING: &[Route] = &[(
        "/get_model_info",
        200,
        r#"{"model_path":"m","is_generation":false}"#,
    )];

    const LLAMA: &[Route] = &[(
        "/props",
        200,
        r#"{"total_slots":4,"chat_template":"{{ messages }}","modalities":{"vision":true,"audio":false,"video":false},"chat_template_caps":{"supports_tools":true,"supports_parallel_tool_calls":false}}"#,
    )];

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn name(raw: &str) -> ModelName {
        ModelName::parse(raw).unwrap()
    }

    #[tokio::test]
    async fn an_ollama_model_reports_the_capabilities_of_its_own_show_answer() {
        let upstream = upstream_serving(OLLAMA).await;
        let found = read(
            &upstream,
            &ids(&["llama3.2:latest"]),
            &mut Memo::default(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            found[&name("llama3.2:latest")],
            vec![ModelTag::Text, ModelTag::Tools, ModelTag::Vision]
        );
    }

    #[tokio::test]
    async fn an_sglang_generation_model_reports_text_and_its_image_input() {
        let upstream = upstream_serving(SGLANG).await;
        let found = read(
            &upstream,
            &ids(&["m"]),
            &mut Memo::default(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(found[&name("m")], vec![ModelTag::Text, ModelTag::Vision]);
    }

    #[tokio::test]
    async fn an_sglang_embedding_model_is_recognised_through_the_renamed_endpoint() {
        let upstream = upstream_serving(SGLANG_EMBEDDING).await;
        let found = read(
            &upstream,
            &ids(&["e"]),
            &mut Memo::default(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(found[&name("e")], vec![ModelTag::Embedding]);
    }

    #[tokio::test]
    async fn a_llama_cpp_server_reports_modalities_and_tool_support_from_props() {
        let upstream = upstream_serving(LLAMA).await;
        let found = read(
            &upstream,
            &ids(&["qwen"]),
            &mut Memo::default(),
            Duration::from_secs(5),
        )
        .await;
        assert_eq!(
            found[&name("qwen")],
            vec![ModelTag::Text, ModelTag::Tools, ModelTag::Vision]
        );
    }

    #[tokio::test]
    async fn a_server_with_no_capability_endpoint_contributes_no_tag() {
        let upstream = upstream_serving(&[]).await;
        let mut memo = Memo::default();
        let found = read(
            &upstream,
            &ids(&["vllm-model"]),
            &mut memo,
            Duration::from_secs(5),
        )
        .await;
        assert!(found.is_empty());
        assert_eq!(memo.family, Some(Family::Other));
    }

    #[tokio::test]
    async fn a_pass_without_budget_asks_nothing_and_remembers_nothing() {
        let upstream = upstream_serving(LLAMA).await;
        let mut memo = Memo::default();
        let found = read(&upstream, &ids(&["qwen"]), &mut memo, Duration::ZERO).await;
        assert!(found.is_empty());
        assert!(memo.family.is_none());
    }

    #[tokio::test]
    async fn a_change_of_announced_ids_starts_a_new_session() {
        let upstream = upstream_serving(LLAMA).await;
        let mut memo = Memo::default();
        read(&upstream, &ids(&["a"]), &mut memo, Duration::from_secs(5)).await;
        let found = read(&upstream, &ids(&["b"]), &mut memo, Duration::from_secs(5)).await;
        assert!(found.contains_key(&name("b")));
        assert!(!found.contains_key(&name("a")));
        assert_eq!(memo.ids, ids(&["b"]));
    }
}
