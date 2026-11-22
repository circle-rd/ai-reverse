//! Capability tags resolved from Hugging Face model metadata.
//!
//! The engine exposes no capabilities, so the agent asks the model's public metadata for its
//! `pipeline_tag` — the normalized classification Hugging Face maintains — instead of guessing
//! from the model name. Ids the hub does not know, and boxes with no egress, get no tags;
//! the operator can declare them with `AGENT_MODEL_TAGS`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, Url, redirect};
use reverse_core::ids::ModelName;
use reverse_core::secret::SecretString;
use reverse_core::wire::{AuthorSafetyClaim, ModelMetadata, ModelTag};
use serde::Deserialize;

use crate::config::Config;
use crate::error::CatalogError;

/// Body cap for one metadata answer; the expanded payload is a few kilobytes.
const MAX_METADATA_BYTES: usize = 256 * 1024;
/// How long an unknown or failed lookup is remembered before asking again.
const NEGATIVE_TTL: Duration = Duration::from_secs(900);

#[derive(Debug, Deserialize)]
struct Metadata {
    #[serde(default)]
    pipeline_tag: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Profile {
    #[serde(default, alias = "avatarUrl")]
    avatar_url: Option<String>,
}

enum Cached {
    Metadata(ModelMetadata),
    Missing(Instant),
}

enum CachedAvatar {
    Url(String),
    Missing(Instant),
}

pub struct Catalog {
    http: Client,
    base: Url,
    token: Option<SecretString>,
    cache: Mutex<HashMap<ModelName, Cached>>,
    owner_cache: Mutex<HashMap<String, CachedAvatar>>,
}

impl Catalog {
    pub fn new(config: &Config) -> Result<Self, CatalogError> {
        let http = Client::builder()
            .timeout(config.hf_timeout)
            .connect_timeout(config.hf_timeout)
            .redirect(redirect::Policy::none())
            .user_agent(concat!("reverse-agent/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base: Url::parse(&config.hf_url.origin())
                .map_err(|e| CatalogError::Url(e.to_string()))?,
            token: config.hf_token.clone(),
            cache: Mutex::new(HashMap::new()),
            owner_cache: Mutex::new(HashMap::new()),
        })
    }

    pub async fn resolve(&self, model: &ModelName) -> Option<ModelMetadata> {
        if let Some(metadata) = self.cached(model) {
            return Some(metadata);
        }
        if self.in_backoff(model) {
            return None;
        }
        match self.fetch(model).await {
            Ok(Some(metadata)) => {
                self.remember(model, Cached::Metadata(metadata.clone()));
                Some(metadata)
            }
            Ok(None) => {
                tracing::debug!(model = %model, "catalog.not_found");
                self.remember(model, Cached::Missing(Instant::now()));
                None
            }
            Err(error) => {
                tracing::warn!(model = %model, %error, "catalog.lookup_failed");
                self.remember(model, Cached::Missing(Instant::now()));
                None
            }
        }
    }

    fn cached(&self, model: &ModelName) -> Option<ModelMetadata> {
        match self.lock().get(model) {
            Some(Cached::Metadata(metadata)) => Some(metadata.clone()),
            _ => None,
        }
    }

    fn in_backoff(&self, model: &ModelName) -> bool {
        match self.lock().get(model) {
            Some(Cached::Missing(at)) => at.elapsed() < NEGATIVE_TTL,
            _ => false,
        }
    }

    fn remember(&self, model: &ModelName, entry: Cached) {
        self.lock().insert(model.clone(), entry);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<ModelName, Cached>> {
        self.cache.lock().expect("catalog cache lock poisoned")
    }

    async fn fetch(&self, model: &ModelName) -> Result<Option<ModelMetadata>, CatalogError> {
        let mut url = self
            .base
            .join(&format!("api/models/{model}"))
            .map_err(|e| CatalogError::Url(e.to_string()))?;
        url.query_pairs_mut()
            .append_pair("expand[]", "pipeline_tag")
            .append_pair("expand[]", "tags");
        let mut request = self.http.get(url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token.expose());
        }
        let owner = hub_owner(model).map(str::to_owned);
        let model_lookup = async move {
            let response = request.send().await?;
            match response.status() {
                StatusCode::OK => {}
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                    return Ok(None);
                }
                status => return Err(CatalogError::Status(status)),
            }
            let body = read_bounded(response, MAX_METADATA_BYTES).await?;
            Ok(Some(serde_json::from_slice::<Metadata>(&body)?))
        };
        let avatar_lookup = async {
            match owner.as_deref() {
                Some(owner) => self.avatar_for(owner).await,
                None => None,
            }
        };
        let (source, avatar_url) = tokio::join!(model_lookup, avatar_lookup);
        let Some(source) = source? else {
            return Ok(None);
        };
        let mut metadata = metadata_from(&source);
        if let Some(owner) = owner {
            metadata.hub_owner = Some(owner);
            metadata.hub_avatar_url = avatar_url;
        }
        Ok(Some(metadata))
    }

    async fn avatar_for(&self, owner: &str) -> Option<String> {
        if let Some(cached) = self.cached_avatar(owner) {
            return cached;
        }
        let result = self.fetch_avatar(owner).await;
        let cached = match result {
            Ok(Some(url)) => CachedAvatar::Url(url),
            Ok(None) => CachedAvatar::Missing(Instant::now()),
            Err(error) => {
                tracing::warn!(owner, %error, "catalog.owner_avatar_lookup_failed");
                CachedAvatar::Missing(Instant::now())
            }
        };
        let avatar = match &cached {
            CachedAvatar::Url(url) => Some(url.clone()),
            CachedAvatar::Missing(_) => None,
        };
        self.owner_cache
            .lock()
            .expect("catalog owner cache lock poisoned")
            .insert(owner.to_owned(), cached);
        avatar
    }

    fn cached_avatar(&self, owner: &str) -> Option<Option<String>> {
        match self
            .owner_cache
            .lock()
            .expect("catalog owner cache lock poisoned")
            .get(owner)
        {
            Some(CachedAvatar::Url(url)) => Some(Some(url.clone())),
            Some(CachedAvatar::Missing(at)) if at.elapsed() < NEGATIVE_TTL => Some(None),
            _ => None,
        }
    }

    async fn fetch_avatar(&self, owner: &str) -> Result<Option<String>, CatalogError> {
        let (user, organization) = tokio::join!(
            self.fetch_profile("users", owner),
            self.fetch_profile("organizations", owner),
        );
        match (user, organization) {
            (Ok(Some(url)), _) | (_, Ok(Some(url))) => Ok(Some(url)),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(None), Ok(None)) => Ok(None),
        }
    }

    async fn fetch_profile(&self, kind: &str, owner: &str) -> Result<Option<String>, CatalogError> {
        let mut url = self
            .base
            .join("api/")
            .map_err(|e| CatalogError::Url(e.to_string()))?;
        url.path_segments_mut()
            .map_err(|_| CatalogError::Url("hub origin cannot accept path segments".into()))?
            .extend([kind, owner, "overview"]);
        let response = self.http.get(url).send().await?;
        match response.status() {
            StatusCode::OK => {
                let body = read_bounded(response, MAX_METADATA_BYTES).await?;
                let profile: Profile = serde_json::from_slice(&body)?;
                Ok(profile
                    .avatar_url
                    .as_deref()
                    .and_then(|raw| self.trusted_avatar_url(raw)))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Ok(None),
            status => Err(CatalogError::Status(status)),
        }
    }

    fn trusted_avatar_url(&self, raw: &str) -> Option<String> {
        let url = Url::parse(raw).ok()?;
        let host = url.host_str()?;
        let trusted_host = self.base.host_str()?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some_and(|port| port != 443)
            || !(is_subdomain(host, trusted_host) || is_subdomain(host, "huggingface.co"))
        {
            return None;
        }
        Some(url.to_string())
    }
}

fn is_subdomain(host: &str, root: &str) -> bool {
    host == root
        || host
            .strip_suffix(root)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

fn hub_owner(model: &ModelName) -> Option<&str> {
    let (owner, repo) = model.as_str().split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/')).then_some(owner)
}

async fn read_bounded(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, CatalogError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > limit {
            return Err(CatalogError::Body(
                "metadata answer is too large".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Maps the hub's normalized classification to our tags.
fn metadata_from(source: &Metadata) -> ModelMetadata {
    ModelMetadata {
        tags: tags_from(source),
        author_safety_claim: safety_claim_from(&source.tags),
        ..ModelMetadata::default()
    }
}

fn safety_claim_from(tags: &[String]) -> Option<AuthorSafetyClaim> {
    let mut uncensored = false;
    let mut abliterated = false;
    for tag in tags {
        match tag.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "uncensored" => uncensored = true,
            "abliterated" => abliterated = true,
            _ => {}
        }
    }
    match (uncensored, abliterated) {
        (true, true) => Some(AuthorSafetyClaim::UncensoredAndAbliterated),
        (true, false) => Some(AuthorSafetyClaim::Uncensored),
        (false, true) => Some(AuthorSafetyClaim::Abliterated),
        (false, false) => None,
    }
}

fn tags_from(metadata: &Metadata) -> Vec<ModelTag> {
    let mut tags = metadata
        .pipeline_tag
        .as_deref()
        .and_then(classify)
        .map(<[ModelTag]>::to_vec)
        .unwrap_or_default();
    // The hub repeats the pipeline tag inside `tags` and also records capabilities
    // such as tool calling there; merge both instead of using `tags` only as a fallback.
    for tag in &metadata.tags {
        if let Some(found) = classify(tag) {
            tags.extend_from_slice(found);
        }
    }
    tags.sort();
    tags.dedup();
    tags
}

fn classify(pipeline_tag: &str) -> Option<&'static [ModelTag]> {
    use ModelTag::{Embedding, Image, Rerank, Speech, Text, Tools, Video, Vision};
    Some(match pipeline_tag {
        "text-generation"
        | "text2text-generation"
        | "fill-mask"
        | "text-classification"
        | "token-classification"
        | "question-answering"
        | "summarization"
        | "translation" => &[Text],
        "feature-extraction" | "sentence-similarity" | "sentence-transformers" => &[Embedding],
        "text-ranking" | "reranking" => &[Rerank],
        "tool-calling" | "function-calling" | "function_calling" | "tool_calling" => &[Tools],
        "image-text-to-text" | "visual-question-answering" | "image-to-text" => &[Text, Vision],
        "audio-text-to-text" => &[Text, Speech],
        "video-text-to-text" => &[Text, Video],
        "any-to-any" => &[Text, Vision, Speech, Video],
        "image-classification"
        | "object-detection"
        | "image-segmentation"
        | "depth-estimation"
        | "zero-shot-image-classification"
        | "zero-shot-object-detection"
        | "image-feature-extraction"
        | "mask-generation"
        | "keypoint-detection" => &[Vision],
        "text-to-image" | "image-to-image" | "unconditional-image-generation" => &[Image],
        "text-to-video"
        | "image-to-video"
        | "video-to-video"
        | "video-classification"
        | "text-to-3d" => &[Video],
        "automatic-speech-recognition"
        | "audio-classification"
        | "text-to-speech"
        | "text-to-audio"
        | "audio-to-audio"
        | "voice-activity-detection" => &[Speech],
        _ => return None,
    })
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
