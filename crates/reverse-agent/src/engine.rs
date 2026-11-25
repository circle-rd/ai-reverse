//! Reads load and capacity from the local inference engine's Prometheus metrics.
//!
//! The engine family is recognised from the metric prefix, so nothing has to be configured.
//! When the engine serves no metrics every reading stays [`UNKNOWN`] and a hint tells the
//! operator how to turn them on.

use std::collections::BTreeMap;
use std::time::Duration;

use http::StatusCode;
use reverse_core::status::{EngineHint, EngineKind, UNKNOWN, UNKNOWN_F64};
use serde::Deserialize;

use crate::upstream::Upstream;

const PROBE_BUDGET: Duration = Duration::from_secs(3);
const MAX_METRICS_BYTES: usize = 4 * 1024 * 1024;
const MAX_SMALL_BYTES: usize = 64 * 1024;

/// One reading of the engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub kind: EngineKind,
    pub version: Option<String>,
    pub metrics_enabled: bool,
    pub max_slots: i32,
    pub running: i32,
    pub queued: i32,
    pub tokens_per_second: f64,
}

impl Reading {
    fn unread() -> Self {
        Self {
            kind: EngineKind::Unknown,
            version: None,
            metrics_enabled: false,
            max_slots: UNKNOWN,
            running: UNKNOWN,
            queued: UNKNOWN,
            tokens_per_second: UNKNOWN_F64,
        }
    }

    /// The reading of an engine that does not answer.
    #[must_use]
    pub fn unavailable(operator_slots: Option<i32>) -> Self {
        let mut reading = Self::unread();
        if let Some(slots) = operator_slots {
            reading.max_slots = slots;
        }
        reading
    }

    /// The hint that explains a missing figure, if any.
    #[must_use]
    pub fn hint(&self) -> Option<EngineHint> {
        if !self.metrics_enabled {
            Some(EngineHint::MetricsDisabled)
        } else if self.max_slots == UNKNOWN {
            Some(EngineHint::CapacityNotExposed)
        } else {
            None
        }
    }
}

/// What earlier probes learned, so an engine that lacks an endpoint is not asked again.
#[derive(Debug, Default)]
pub struct Memo {
    props_supported: Option<bool>,
    version_tried: bool,
    version: Option<String>,
}

impl Memo {
    /// Forgets everything: the engine may come back as a different server.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// Reads the engine once. `operator_slots` is the capacity the operator declared, which wins
/// over anything the engine reports.
pub async fn read(upstream: &Upstream, memo: &mut Memo, operator_slots: Option<i32>) -> Reading {
    let mut reading = Reading::unread();

    match upstream
        .get_text("/metrics", MAX_METRICS_BYTES, PROBE_BUDGET)
        .await
    {
        Ok((StatusCode::OK, text)) => {
            reading.metrics_enabled = true;
            apply_metrics(&mut reading, &parse_metrics(&text));
        }
        Ok((status, _)) => tracing::debug!(status = status.as_u16(), "engine.metrics_unavailable"),
        Err(error) => tracing::debug!(%error, "engine.metrics_failed"),
    }

    if matches!(reading.kind, EngineKind::Unknown | EngineKind::LlamaCpp)
        && memo.props_supported != Some(false)
    {
        match upstream
            .get_text("/props", MAX_SMALL_BYTES, PROBE_BUDGET)
            .await
        {
            Ok((StatusCode::OK, text)) => match parse_props(&text) {
                Some(props) => {
                    memo.props_supported = Some(true);
                    reading.kind = EngineKind::LlamaCpp;
                    reading.max_slots = props.slots;
                    memo.version = props.version;
                }
                None => memo.props_supported = Some(false),
            },
            Ok(_) => memo.props_supported = Some(false),
            Err(error) => tracing::debug!(%error, "engine.props_failed"),
        }
    }

    if !memo.version_tried && matches!(reading.kind, EngineKind::Sglang | EngineKind::Vllm) {
        memo.version_tried = true;
        if let Ok((StatusCode::OK, text)) = upstream
            .get_text("/version", MAX_SMALL_BYTES, PROBE_BUDGET)
            .await
        {
            memo.version = parse_version(&text);
        }
    }
    reading.version.clone_from(&memo.version);

    if let Some(slots) = operator_slots {
        reading.max_slots = slots;
    }
    reading
}

fn apply_metrics(reading: &mut Reading, samples: &BTreeMap<String, f64>) {
    let gauge = |name: &str| samples.get(name).copied();
    let count = |name: &str| gauge(name).map_or(UNKNOWN, to_count);
    let rate = |name: &str| gauge(name).map_or(UNKNOWN_F64, |v| v.max(0.0));
    if samples.keys().any(|name| name.starts_with("sglang:")) {
        reading.kind = EngineKind::Sglang;
        reading.running = count("sglang:num_running_reqs");
        reading.queued = count("sglang:num_queue_reqs");
        reading.tokens_per_second = rate("sglang:gen_throughput");
    } else if samples.keys().any(|name| name.starts_with("vllm:")) {
        reading.kind = EngineKind::Vllm;
        reading.running = count("vllm:num_requests_running");
        reading.queued = count("vllm:num_requests_waiting");
    } else if samples.keys().any(|name| name.starts_with("llamacpp:")) {
        reading.kind = EngineKind::LlamaCpp;
        reading.running = count("llamacpp:requests_processing");
        reading.queued = count("llamacpp:requests_deferred");
        reading.tokens_per_second = rate("llamacpp:predicted_tokens_seconds");
    }
}

#[allow(clippy::cast_possible_truncation)]
fn to_count(value: f64) -> i32 {
    value.round().clamp(0.0, f64::from(i32::MAX)) as i32
}

/// Parses the Prometheus text format into the largest value seen per metric name.
///
/// Labels are dropped: engines repeat the same gauge once per tensor-parallel rank and every
/// rank reports the same load, so the maximum is the figure, not the sum.
fn parse_metrics(text: &str) -> BTreeMap<String, f64> {
    let mut samples = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(end) = line.find(['{', ' ']) else {
            continue;
        };
        let (name, rest) = line.split_at(end);
        let rest = rest.rsplit_once('}').map_or(rest, |(_, after)| after);
        let Some(value) = rest
            .split_whitespace()
            .next()
            .and_then(|raw| raw.parse::<f64>().ok())
            .filter(|value| value.is_finite())
        else {
            continue;
        };
        samples
            .entry(name.to_owned())
            .and_modify(|seen: &mut f64| *seen = seen.max(value))
            .or_insert(value);
    }
    samples
}

struct Props {
    slots: i32,
    version: Option<String>,
}

#[derive(Deserialize)]
struct PropsBody {
    total_slots: u32,
    #[serde(default)]
    build_info: Option<String>,
}

fn parse_props(text: &str) -> Option<Props> {
    let body: PropsBody = serde_json::from_str(text).ok()?;
    Some(Props {
        slots: i32::try_from(body.total_slots).ok()?,
        version: body.build_info.and_then(|v| bounded(&v)),
    })
}

#[derive(Deserialize)]
struct VersionBody {
    version: String,
}

fn parse_version(text: &str) -> Option<String> {
    let body: VersionBody = serde_json::from_str(text).ok()?;
    bounded(&body.version)
}

/// A version is shown to people, so it stays short and printable.
fn bounded(raw: &str) -> Option<String> {
    let value = raw.trim();
    (!value.is_empty() && value.len() <= 64 && value.chars().all(|c| !c.is_control()))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::upstream_serving;

    // Taken from an SGLang 0.5 server started with `--enable-metrics`.
    const SGLANG: &str = r#"# HELP sglang:num_running_reqs The number of running requests.
# TYPE sglang:num_running_reqs gauge
sglang:num_running_reqs{engine_type="unified",model_name="m",tp_rank="0"} 3.0
sglang:num_queue_reqs{engine_type="unified",model_name="m",tp_rank="0"} 1.0
sglang:gen_throughput{engine_type="unified",model_name="m",tp_rank="0"} 41.5
sglang:max_total_num_tokens{engine_type="unified",model_name="m",tp_rank="0"} 375680.0
"#;

    fn reading(text: &str) -> Reading {
        let mut reading = Reading::unread();
        reading.metrics_enabled = true;
        apply_metrics(&mut reading, &parse_metrics(text));
        reading
    }

    #[test]
    fn sglang_load_is_read_from_its_gauges() {
        let r = reading(SGLANG);
        assert_eq!(r.kind, EngineKind::Sglang);
        assert_eq!((r.running, r.queued), (3, 1));
        assert!((r.tokens_per_second - 41.5).abs() < f64::EPSILON);
        assert_eq!(r.max_slots, UNKNOWN);
        assert_eq!(r.hint(), Some(EngineHint::CapacityNotExposed));
    }

    #[test]
    fn llama_cpp_load_is_read_from_its_gauges() {
        let r = reading(
            "llamacpp:requests_processing 1\nllamacpp:requests_deferred 2\nllamacpp:predicted_tokens_seconds 17.25\n",
        );
        assert_eq!(r.kind, EngineKind::LlamaCpp);
        assert_eq!((r.running, r.queued), (1, 2));
    }

    #[test]
    fn vllm_load_is_read_from_its_gauges() {
        let r = reading(
            "vllm:num_requests_running{model_name=\"m\"} 5.0\nvllm:num_requests_waiting{model_name=\"m\"} 0.0\n",
        );
        assert_eq!(r.kind, EngineKind::Vllm);
        assert_eq!((r.running, r.queued), (5, 0));
        assert!(r.tokens_per_second < 0.0);
    }

    #[test]
    fn ranks_repeating_one_gauge_are_not_summed() {
        let samples = parse_metrics(
            "sglang:num_running_reqs{tp_rank=\"0\"} 4.0\nsglang:num_running_reqs{tp_rank=\"1\"} 4.0\n",
        );
        assert!((samples["sglang:num_running_reqs"] - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn metrics_without_a_known_prefix_identify_no_engine() {
        let r = reading("process_cpu_seconds_total 12.5\n");
        assert_eq!(r.kind, EngineKind::Unknown);
        assert_eq!(r.running, UNKNOWN);
    }

    #[test]
    fn a_missing_gauge_is_unknown_not_zero() {
        let r = reading("sglang:num_running_reqs 2\n");
        assert_eq!(r.queued, UNKNOWN);
    }

    #[test]
    fn nan_and_garbage_samples_are_ignored() {
        let samples = parse_metrics("sglang:num_running_reqs NaN\nnot a metric line\n# comment\n");
        assert!(samples.is_empty());
    }

    #[test]
    fn disabled_metrics_hint_how_to_enable_them() {
        assert_eq!(Reading::unread().hint(), Some(EngineHint::MetricsDisabled));
        let mut known = reading(SGLANG);
        known.max_slots = 8;
        assert_eq!(known.hint(), None);
    }

    #[test]
    fn llama_cpp_props_give_the_slot_count() {
        let props = parse_props(r#"{"total_slots": 4, "build_info": "b6000-abc"}"#).unwrap();
        assert_eq!(props.slots, 4);
        assert_eq!(props.version.as_deref(), Some("b6000-abc"));
        assert!(parse_props(r#"{"model": "x"}"#).is_none());
    }

    #[test]
    fn a_version_with_control_characters_is_dropped() {
        assert_eq!(
            parse_version(r#"{"version":"0.5.1"}"#).as_deref(),
            Some("0.5.1")
        );
        assert_eq!(parse_version("{\"version\":\"1\\u0007\"}"), None);
    }

    #[tokio::test]
    async fn an_sglang_engine_is_recognised_and_the_operator_declares_its_capacity() {
        let upstream = upstream_serving(&[
            ("/metrics", 200, SGLANG),
            ("/version", 200, r#"{"version":"0.5.1"}"#),
        ])
        .await;
        let r = read(&upstream, &mut Memo::default(), Some(8)).await;
        assert_eq!(r.kind, EngineKind::Sglang);
        assert_eq!((r.running, r.queued, r.max_slots), (3, 1, 8));
        assert_eq!(r.version.as_deref(), Some("0.5.1"));
        assert_eq!(r.hint(), None);
    }

    #[tokio::test]
    async fn llama_cpp_without_metrics_still_reports_its_slots_and_asks_for_metrics() {
        let upstream = upstream_serving(&[
            ("/metrics", 501, r#"{"error":"metrics disabled"}"#),
            ("/props", 200, r#"{"total_slots":4,"build_info":"b6000"}"#),
        ])
        .await;
        let r = read(&upstream, &mut Memo::default(), None).await;
        assert_eq!(r.kind, EngineKind::LlamaCpp);
        assert_eq!(r.max_slots, 4);
        assert_eq!(r.running, UNKNOWN);
        assert_eq!(r.version.as_deref(), Some("b6000"));
        assert_eq!(r.hint(), Some(EngineHint::MetricsDisabled));
    }

    #[tokio::test]
    async fn a_silent_engine_is_unknown_with_a_hint_and_is_not_asked_for_props_twice() {
        let upstream = upstream_serving(&[]).await;
        let mut memo = Memo::default();
        let r = read(&upstream, &mut memo, None).await;
        assert_eq!(r.kind, EngineKind::Unknown);
        assert_eq!(r.max_slots, UNKNOWN);
        assert_eq!(r.hint(), Some(EngineHint::MetricsDisabled));
        assert_eq!(memo.props_supported, Some(false));
    }
}
