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
use reverse_core::wire::ModelTag;
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

enum Cached {
    Tags(Vec<ModelTag>),
    Missing(Instant),
}

/// Resolves and caches model tags from the Hugging Face hub.
pub struct Catalog {
    http: Client,
    base: Url,
    token: Option<SecretString>,
    cache: Mutex<HashMap<ModelName, Cached>>,
}

impl Catalog {
    /// Builds a client for the configured hub endpoint.
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
        })
    }

    /// The tags of one model, or `None` when the hub has no usable metadata.
    pub async fn resolve(&self, model: &ModelName) -> Option<Vec<ModelTag>> {
        if let Some(tags) = self.cached(model) {
            return Some(tags);
        }
        if self.in_backoff(model) {
            return None;
        }
        match self.fetch(model).await {
            Ok(Some(tags)) => {
                self.remember(model, Cached::Tags(tags.clone()));
                Some(tags)
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

    fn cached(&self, model: &ModelName) -> Option<Vec<ModelTag>> {
        match self.lock().get(model) {
            Some(Cached::Tags(tags)) => Some(tags.clone()),
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

    async fn fetch(&self, model: &ModelName) -> Result<Option<Vec<ModelTag>>, CatalogError> {
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
        let response = request.send().await?;
        match response.status() {
            StatusCode::OK => {}
            // A private or gated repository, or an id the hub does not have.
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Ok(None);
            }
            status => return Err(CatalogError::Status(status)),
        }
        let body = read_bounded(response, MAX_METADATA_BYTES).await?;
        let metadata: Metadata = serde_json::from_slice(&body)?;
        Ok(Some(tags_from(&metadata)))
    }
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
mod tests {
    use std::convert::Infallible;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use http::{Request, Response, StatusCode, header};
    use http_body_util::Full;
    use hyper::body::Incoming;
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    use super::*;

    fn model(name: &str) -> ModelName {
        ModelName::parse(name).unwrap()
    }

    fn metadata(pipeline_tag: Option<&str>, tags: &[&str]) -> Metadata {
        Metadata {
            pipeline_tag: pipeline_tag.map(str::to_owned),
            tags: tags.iter().map(|t| (*t).to_owned()).collect(),
        }
    }

    #[test]
    fn the_hub_pipeline_tag_is_the_primary_source() {
        assert_eq!(
            tags_from(&metadata(Some("feature-extraction"), &[])),
            vec![ModelTag::Embedding]
        );
        assert_eq!(
            tags_from(&metadata(Some("text-generation"), &[])),
            vec![ModelTag::Text]
        );
        assert_eq!(
            tags_from(&metadata(Some("image-text-to-text"), &[])),
            vec![ModelTag::Text, ModelTag::Vision]
        );
        assert_eq!(
            tags_from(&metadata(Some("text-to-image"), &[])),
            vec![ModelTag::Image]
        );
        assert_eq!(
            tags_from(&metadata(Some("text-ranking"), &[])),
            vec![ModelTag::Rerank]
        );
        assert_eq!(
            tags_from(&metadata(Some("automatic-speech-recognition"), &[])),
            vec![ModelTag::Speech]
        );
    }

    #[test]
    fn tags_are_a_fallback_and_unknown_models_get_nothing() {
        assert_eq!(
            tags_from(&metadata(
                None,
                &["sentence-transformers", "sentence-similarity"]
            )),
            vec![ModelTag::Embedding]
        );
        assert!(tags_from(&metadata(None, &["region:us"])).is_empty());
        assert!(tags_from(&metadata(Some("custom-unknown-tag"), &[])).is_empty());
    }

    #[test]
    fn hub_tags_add_capabilities_beyond_the_pipeline_tag() {
        assert_eq!(
            tags_from(&metadata(Some("text-generation"), &["tool-calling"])),
            vec![ModelTag::Text, ModelTag::Tools]
        );
        assert_eq!(
            tags_from(&metadata(None, &["audio-text-to-text"])),
            vec![ModelTag::Text, ModelTag::Speech]
        );
    }

    struct FakeHub {
        addr: SocketAddr,
        hits: Arc<AtomicUsize>,
    }

    async fn hub(status: StatusCode, body: &'static str) -> FakeHub {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let counter = counter.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |request: Request<Incoming>| {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let path = request.uri().path().to_owned();
                        async move {
                            let reply =
                                if path.ends_with("/api/models/perplexity-ai/pplx-embed-v1-0.6b") {
                                    Response::builder()
                                        .status(status)
                                        .header(header::CONTENT_TYPE, "application/json")
                                        .body(Full::new(Bytes::from(body)))
                                } else {
                                    Response::builder()
                                        .status(StatusCode::UNAUTHORIZED)
                                        .body(Full::new(Bytes::new()))
                                };
                            Ok::<_, Infallible>(reply.unwrap())
                        }
                    });
                    let _ = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        });
        FakeHub { addr, hits }
    }

    fn config(endpoint: &str) -> Config {
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
            ("AGENT_HF_URL".to_owned(), endpoint.to_owned()),
        ]);
        Config::from_env(&reverse_core::config::Env::new(move |key| {
            map.get(key).cloned()
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn a_known_model_is_resolved_once_and_cached() {
        let hub = hub(
            StatusCode::OK,
            r#"{"pipeline_tag":"feature-extraction","tags":["feature-extraction"]}"#,
        )
        .await;
        let catalog = Catalog::new(&config(&format!("http://{}", hub.addr))).unwrap();
        let name = model("perplexity-ai/pplx-embed-v1-0.6b");

        assert_eq!(
            catalog.resolve(&name).await,
            Some(vec![ModelTag::Embedding])
        );
        assert_eq!(
            catalog.resolve(&name).await,
            Some(vec![ModelTag::Embedding])
        );
        assert_eq!(hub.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_unknown_model_is_not_refetched_within_the_backoff() {
        let hub = hub(StatusCode::UNAUTHORIZED, "").await;
        let catalog = Catalog::new(&config(&format!("http://{}", hub.addr))).unwrap();
        let name = model("perplexity-ai/pplx-embed-v1-0.6b");

        assert_eq!(catalog.resolve(&name).await, None);
        assert_eq!(catalog.resolve(&name).await, None);
        assert_eq!(hub.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    #[ignore = "needs egress to huggingface.co"]
    async fn the_real_hub_classifies_the_embedding_model() {
        let catalog = Catalog::new(&config("https://huggingface.co")).unwrap();
        assert_eq!(
            catalog
                .resolve(&model("perplexity-ai/pplx-embed-v1-0.6b"))
                .await,
            Some(vec![ModelTag::Embedding])
        );
    }
}
