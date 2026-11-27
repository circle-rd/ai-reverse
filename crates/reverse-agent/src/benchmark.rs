//! Measures the speed of the local engine once, with one short fixed generation.
//!
//! The figure is an indication (one stream, a tiny prompt), good enough to price electricity
//! per token, not a throughput benchmark. It never blocks serving and never repairs anything:
//! when a run fails the speed stays unknown.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use http_body_util::BodyExt;
use reverse_core::body::full;
use reverse_core::shutdown::triggered;
use reverse_core::status::{Benchmark, UNKNOWN_F64};
use reverse_core::wire::{ModelTag, ModelsSnapshot};
use serde_json::Value;
use tokio::sync::watch;

use crate::error::BenchmarkError;
use crate::upstream::Upstream;

const PROMPT: &str = "Say hello.";
const MAX_TOKENS: u32 = 64;
const RUN_BUDGET: Duration = Duration::from_secs(120);
const MAX_ATTEMPTS: u32 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(30);
const MAX_BODY_BYTES: usize = 1024 * 1024;

/// Waits for a text model, measures it, publishes the result. Runs once per process.
pub async fn run(
    upstream: Arc<Upstream>,
    mut models: watch::Receiver<ModelsSnapshot>,
    results: watch::Sender<Benchmark>,
    mut shutdown: watch::Receiver<bool>,
) {
    let model = loop {
        if let Some(model) = pick_model(&models.borrow()) {
            break model;
        }
        tokio::select! {
            changed = models.changed() => if changed.is_err() { return },
            () = triggered(&mut shutdown) => return,
        }
    };

    for attempt in 1..=MAX_ATTEMPTS {
        match measure(&upstream, &model).await {
            Ok(benchmark) => {
                tracing::info!(
                    %model,
                    ttft_ms = benchmark.ttft_ms,
                    tokens_per_second = benchmark.tokens_per_second,
                    "benchmark.done"
                );
                results.send_replace(benchmark);
                return;
            }
            Err(error) => tracing::warn!(%model, attempt, %error, "benchmark.failed"),
        }
        tokio::select! {
            () = tokio::time::sleep(RETRY_DELAY) => {}
            () = triggered(&mut shutdown) => return,
        }
    }
}

/// The first text model of a reachable engine.
fn pick_model(snapshot: &ModelsSnapshot) -> Option<String> {
    if !snapshot.engine_available {
        return None;
    }
    snapshot
        .models
        .iter()
        .find(|entry| entry.metadata.tags.contains(&ModelTag::Text))
        .map(|entry| entry.id.clone())
}

async fn measure(upstream: &Upstream, model: &str) -> Result<Benchmark, BenchmarkError> {
    let body = serde_json::to_vec(&serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": PROMPT }],
        "max_tokens": MAX_TOKENS,
        "temperature": 0,
        "stream": true,
        "stream_options": { "include_usage": true },
    }))
    .map_err(|e| BenchmarkError::Stream(e.to_string()))?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );

    let started = Instant::now();
    let work = async {
        let response = upstream
            .send(
                Method::POST,
                "/v1/chat/completions",
                headers,
                full(Bytes::from(body)),
            )
            .await?;
        if response.status() != StatusCode::OK {
            return Err(BenchmarkError::Status(response.status()));
        }
        let mut stream = response.into_body();
        let mut lines = SseLines::default();
        let mut run = Run::new(started);
        let mut seen = 0_usize;
        while let Some(frame) = stream.frame().await {
            let frame = frame.map_err(|e| BenchmarkError::Stream(e.to_string()))?;
            let Some(data) = frame.data_ref() else {
                continue;
            };
            seen += data.len();
            if seen > MAX_BODY_BYTES {
                return Err(BenchmarkError::Stream("answer too large".to_owned()));
            }
            let now = Instant::now();
            for payload in lines.feed(data) {
                run.observe(&payload, now);
            }
        }
        Ok(run)
    };
    let run = tokio::time::timeout(RUN_BUDGET, work)
        .await
        .map_err(|_| BenchmarkError::Timeout(RUN_BUDGET))??;
    run.finish(model)
}

/// Splits a server-sent-events byte stream into the payloads of its `data:` lines.
#[derive(Default)]
struct SseLines {
    pending: Vec<u8>,
}

impl SseLines {
    fn feed(&mut self, bytes: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(bytes);
        let mut payloads = Vec::new();
        while let Some(end) = self.pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(payload) = line.trim_end().strip_prefix("data:") {
                payloads.push(payload.trim_start().to_owned());
            }
        }
        payloads
    }
}

/// Timestamps of one streamed generation.
struct Run {
    started: Instant,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    completion_tokens: Option<u64>,
}

impl Run {
    fn new(started: Instant) -> Self {
        Self {
            started,
            first_token: None,
            last_token: None,
            completion_tokens: None,
        }
    }

    fn observe(&mut self, payload: &str, now: Instant) {
        let Ok(chunk) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        let delta = &chunk["choices"][0]["delta"];
        let generated = ["content", "reasoning_content", "reasoning"]
            .iter()
            .any(|key| delta[*key].as_str().is_some_and(|text| !text.is_empty()));
        if generated {
            self.first_token.get_or_insert(now);
            self.last_token = Some(now);
        }
        if let Some(tokens) = chunk["usage"]["completion_tokens"].as_u64() {
            self.completion_tokens = Some(tokens);
        }
    }

    fn finish(self, model: &str) -> Result<Benchmark, BenchmarkError> {
        let (Some(first), Some(last)) = (self.first_token, self.last_token) else {
            return Err(BenchmarkError::NoTokens);
        };
        let ttft_ms =
            i32::try_from(first.duration_since(self.started).as_millis()).unwrap_or(i32::MAX);
        let decode = last.duration_since(first).as_secs_f64();
        // The first token ends the prefill; only the tokens after it measure decoding.
        #[allow(clippy::cast_precision_loss)]
        let tokens_per_second = match self.completion_tokens {
            Some(tokens) if tokens > 1 && decode > 0.0 => (tokens - 1) as f64 / decode,
            _ => UNKNOWN_F64,
        };
        Ok(Benchmark {
            model: Some(model.to_owned()),
            ttft_ms: ttft_ms.max(0),
            tokens_per_second,
        })
    }
}

#[cfg(test)]
mod tests {
    use reverse_core::status::UNKNOWN;
    use reverse_core::wire::{ModelEntry, ModelMetadata};

    use super::*;

    fn entry(id: &str, tags: &[ModelTag]) -> ModelEntry {
        ModelEntry {
            id: id.to_owned(),
            metadata: ModelMetadata {
                tags: tags.to_vec(),
                ..ModelMetadata::default()
            },
        }
    }

    #[test]
    fn lines_split_across_frames_are_reassembled() {
        let mut lines = SseLines::default();
        assert!(lines.feed(b"data: {\"a\"").is_empty());
        assert_eq!(
            lines.feed(b":1}\n\ndata: [DONE]\n"),
            ["{\"a\":1}", "[DONE]"]
        );
    }

    #[test]
    fn the_run_measures_time_to_first_token_then_decode_speed() {
        let t0 = Instant::now();
        let mut run = Run::new(t0);
        run.observe(
            r#"{"choices":[{"delta":{"role":"assistant","content":""}}]}"#,
            t0 + Duration::from_millis(50),
        );
        run.observe(
            r#"{"choices":[{"delta":{"content":"Hi"}}]}"#,
            t0 + Duration::from_millis(400),
        );
        run.observe(
            r#"{"choices":[{"delta":{"content":"!"}}]}"#,
            t0 + Duration::from_millis(1400),
        );
        run.observe(
            r#"{"choices":[],"usage":{"completion_tokens":21}}"#,
            t0 + Duration::from_millis(1401),
        );
        let result = run.finish("m").unwrap();
        assert_eq!(result.ttft_ms, 400);
        // 20 tokens after the first one in one second.
        assert!((result.tokens_per_second - 20.0).abs() < 0.01);
        assert_eq!(result.model.as_deref(), Some("m"));
    }

    #[test]
    fn reasoning_tokens_count_as_the_first_token() {
        let t0 = Instant::now();
        let mut run = Run::new(t0);
        run.observe(
            r#"{"choices":[{"delta":{"reasoning_content":"hm"}}]}"#,
            t0 + Duration::from_millis(100),
        );
        assert_eq!(run.finish("m").unwrap().ttft_ms, 100);
    }

    #[test]
    fn without_usage_the_speed_is_unknown_not_guessed() {
        let t0 = Instant::now();
        let mut run = Run::new(t0);
        run.observe(
            r#"{"choices":[{"delta":{"content":"a"}}]}"#,
            t0 + Duration::from_millis(100),
        );
        run.observe(
            r#"{"choices":[{"delta":{"content":"b"}}]}"#,
            t0 + Duration::from_millis(200),
        );
        let result = run.finish("m").unwrap();
        assert_eq!(result.tokens_per_second, UNKNOWN_F64);
        assert_ne!(result.ttft_ms, UNKNOWN);
    }

    #[test]
    fn an_answer_without_generated_text_is_a_failure() {
        let mut run = Run::new(Instant::now());
        run.observe(
            r#"{"choices":[{"delta":{"role":"assistant"}}]}"#,
            Instant::now(),
        );
        assert!(matches!(run.finish("m"), Err(BenchmarkError::NoTokens)));
    }

    #[tokio::test]
    async fn a_streamed_completion_is_measured_end_to_end() {
        let upstream = crate::test_support::upstream_serving(&[(
            "/v1/chat/completions",
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n\ndata: {\"choices\":[],\"usage\":{\"completion_tokens\":2}}\n\ndata: [DONE]\n\n",
        )])
        .await;
        let result = measure(&upstream, "chat").await.unwrap();
        assert_eq!(result.model.as_deref(), Some("chat"));
        assert!(result.ttft_ms >= 0);
    }

    #[tokio::test]
    async fn an_engine_that_refuses_the_test_request_leaves_the_speed_unknown() {
        let upstream = crate::test_support::upstream_serving(&[(
            "/v1/chat/completions",
            400,
            "{\"error\":\"nope\"}",
        )])
        .await;
        assert!(matches!(
            measure(&upstream, "chat").await,
            Err(BenchmarkError::Status(StatusCode::BAD_REQUEST))
        ));
    }

    #[test]
    fn only_a_text_model_of_a_reachable_engine_is_measured() {
        let snapshot = ModelsSnapshot {
            revision: 1,
            engine_available: true,
            models: vec![
                entry("embed", &[ModelTag::Embedding]),
                entry("chat", &[ModelTag::Text, ModelTag::Tools]),
            ],
        };
        assert_eq!(pick_model(&snapshot).as_deref(), Some("chat"));
        let down = ModelsSnapshot {
            engine_available: false,
            ..snapshot
        };
        assert_eq!(pick_model(&down), None);
    }
}
