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
fn safety_claims_require_exact_hub_tags() {
    assert_eq!(
        safety_claim_from(&["UNCENSORED".to_owned(), "abliterated".to_owned()]),
        Some(AuthorSafetyClaim::UncensoredAndAbliterated)
    );
    assert_eq!(
        safety_claim_from(&["uncensored".to_owned()]),
        Some(AuthorSafetyClaim::Uncensored)
    );
    assert_eq!(
        safety_claim_from(&["abliterated".to_owned()]),
        Some(AuthorSafetyClaim::Abliterated)
    );
    assert_eq!(
        safety_claim_from(&["uncensored-model".to_owned(), "unfiltered".to_owned()]),
        None
    );
}

#[test]
fn only_https_avatar_urls_on_trusted_hub_domains_are_accepted() {
    let catalog = Catalog::new(&config("https://huggingface.co")).unwrap();
    assert_eq!(
        catalog.trusted_avatar_url("https://cdn-avatars.huggingface.co/avatar.png"),
        Some("https://cdn-avatars.huggingface.co/avatar.png".to_owned())
    );
    for url in [
        "http://huggingface.co/avatar.png",
        "https://huggingface.co.attacker.test/avatar.png",
        "https://attacker.test/avatar.png",
        "https://user@huggingface.co/avatar.png",
        "https://huggingface.co:444/avatar.png",
    ] {
        assert_eq!(catalog.trusted_avatar_url(url), None, "{url}");
    }
}

#[test]
fn hub_owners_require_one_namespace_segment() {
    assert_eq!(hub_owner(&model("Org/model")), Some("Org"));
    assert_eq!(hub_owner(&model("local-model")), None);
    assert_eq!(hub_owner(&model("Org/sub/model")), None);
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
                        let reply = if path.ends_with("/users/perplexity-ai/overview") {
                            Response::builder()
                                .status(StatusCode::OK)
                                .header(header::CONTENT_TYPE, "application/json")
                                .body(Full::new(Bytes::from(
                                    r#"{"avatarUrl":"https://127.0.0.1/avatar.png"}"#,
                                )))
                        } else if path == "/api/organizations/perplexity-ai/overview" {
                            Response::builder()
                                .status(StatusCode::NOT_FOUND)
                                .body(Full::new(Bytes::new()))
                        } else if path.starts_with("/api/models/perplexity-ai/") {
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

    let expected = ModelMetadata {
        tags: vec![ModelTag::Embedding],
        hub_owner: Some("perplexity-ai".to_owned()),
        hub_avatar_url: Some("https://127.0.0.1/avatar.png".to_owned()),
        ..ModelMetadata::default()
    };
    assert_eq!(catalog.resolve(&name).await, Some(expected.clone()));
    assert_eq!(catalog.resolve(&name).await, Some(expected));
    assert_eq!(hub.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn an_owner_avatar_is_cached_across_models() {
    let hub = hub(
        StatusCode::OK,
        r#"{"pipeline_tag":"text-generation","tags":[]}"#,
    )
    .await;
    let catalog = Catalog::new(&config(&format!("http://{}", hub.addr))).unwrap();
    let first = model("perplexity-ai/model-a");
    let second = model("perplexity-ai/model-b");

    assert!(catalog.resolve(&first).await.is_some());
    assert!(catalog.resolve(&second).await.is_some());
    assert_eq!(hub.hits.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn an_unknown_model_is_not_refetched_within_the_backoff() {
    let hub = hub(StatusCode::UNAUTHORIZED, "").await;
    let catalog = Catalog::new(&config(&format!("http://{}", hub.addr))).unwrap();
    let name = model("perplexity-ai/pplx-embed-v1-0.6b");

    assert_eq!(catalog.resolve(&name).await, None);
    assert_eq!(catalog.resolve(&name).await, None);
    assert_eq!(hub.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
#[ignore = "needs egress to huggingface.co"]
async fn the_real_hub_classifies_the_embedding_model() {
    let catalog = Catalog::new(&config("https://huggingface.co")).unwrap();
    let metadata = catalog
        .resolve(&model("perplexity-ai/pplx-embed-v1-0.6b"))
        .await
        .unwrap();
    assert_eq!(metadata.tags, vec![ModelTag::Embedding]);
    assert_eq!(metadata.hub_owner.as_deref(), Some("perplexity-ai"));
    assert!(metadata.hub_avatar_url.is_some());
}
