//! Live status of an agent, its engine and its host, served at [`crate::wire::AGENT_STATUS_PATH`].
//!
//! Every figure the agent cannot read is [`UNKNOWN`] (`-1`), never `0`: `0` is a real value
//! (an idle GPU) and a consumer must be able to tell the two apart.

use serde::{Deserialize, Serialize};

/// Marks a figure the agent could not read.
pub const UNKNOWN: i32 = -1;

/// [`UNKNOWN`] for fractional figures.
pub const UNKNOWN_F64: f64 = -1.0;

/// How much room the engine has for another request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServingState {
    /// The engine does not answer.
    Offline,
    /// Nothing is running.
    Idle,
    /// Requests are running and there is room for more.
    Busy,
    /// No room left: a new request would wait or be refused.
    Occupied,
}

impl ServingState {
    /// Derives the state from a load reading; `slots` and `running` are [`UNKNOWN`] when unread.
    ///
    /// Without a known capacity the engine is `Occupied` only when it queues requests, which is
    /// the one saturation signal that needs no capacity.
    #[must_use]
    pub fn from_load(available: bool, running: i32, queued: i32, slots: i32) -> Self {
        if !available {
            return Self::Offline;
        }
        let running = running.max(0);
        let queued = queued.max(0);
        if queued > 0 || (slots > 0 && running >= slots) {
            Self::Occupied
        } else if running > 0 {
            Self::Busy
        } else {
            Self::Idle
        }
    }
}

/// The inference server family, recognised from the metrics it exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineKind {
    /// SGLang.
    Sglang,
    /// vLLM.
    Vllm,
    /// llama.cpp `llama-server`.
    LlamaCpp,
    /// Not recognised, or metrics are off and nothing else identifies it.
    Unknown,
}

/// Why a figure is missing and what the operator can do about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EngineHint {
    /// The engine serves no metrics: start it with its metrics option (`--metrics`,
    /// `--enable-metrics`).
    MetricsDisabled,
    /// Metrics are on but the engine does not report its request capacity: set
    /// `AGENT_MAX_SLOTS` on the agent.
    CapacityNotExposed,
}

/// What the agent reads from the inference engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    /// The server family.
    pub kind: EngineKind,
    /// Version string the engine reports, if it reports one.
    pub version: Option<String>,
    /// Whether the last poll of the engine succeeded.
    pub available: bool,
    /// Whether the engine exposes a metrics endpoint.
    pub metrics_enabled: bool,
    /// Concurrent requests the engine accepts, or [`UNKNOWN`].
    pub max_slots: i32,
    /// Requests the engine is running, or [`UNKNOWN`].
    pub running_requests: i32,
    /// Requests the engine holds in its queue, or [`UNKNOWN`].
    pub queued_requests: i32,
    /// Generation throughput the engine reports right now, or [`UNKNOWN_F64`].
    pub tokens_per_second: f64,
    /// How to obtain a missing figure.
    pub hint: Option<EngineHint>,
}

/// Load of the host machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    /// Seconds since the machine booted.
    pub uptime_secs: u64,
    /// CPU busy share since the previous sample, 0 to 100, or [`UNKNOWN`].
    pub cpu_load_percent: i32,
    /// Share of memory in use, 0 to 100, or [`UNKNOWN`].
    pub memory_used_percent: i32,
    /// Hottest thermal zone in degrees Celsius, or [`UNKNOWN`].
    pub cpu_temperature_c: i32,
}

/// Load of one GPU.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuStatus {
    /// Name the driver reports.
    pub name: String,
    /// Compute utilisation, 0 to 100, or [`UNKNOWN`].
    pub utilization_percent: i32,
    /// Memory in use, 0 to 100, or [`UNKNOWN`]. On a unified-memory machine this is the share of
    /// the system memory, see `memory_shared`.
    pub memory_used_percent: i32,
    /// Whether the GPU shares its memory with the CPU (DGX Spark).
    pub memory_shared: bool,
    /// Die temperature in degrees Celsius, or [`UNKNOWN`].
    pub temperature_c: i32,
    /// Power draw in watts, or [`UNKNOWN_F64`].
    pub power_watts: f64,
}

/// Speed of one short generation the agent ran against its own engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Benchmark {
    /// The model that was measured; `None` until a run finished.
    pub model: Option<String>,
    /// Milliseconds to the first generated token, or [`UNKNOWN`].
    pub ttft_ms: i32,
    /// Generation speed after the first token, or [`UNKNOWN_F64`].
    pub tokens_per_second: f64,
}

impl Benchmark {
    /// No run has finished.
    #[must_use]
    pub fn unknown() -> Self {
        Self {
            model: None,
            ttft_ms: UNKNOWN,
            tokens_per_second: UNKNOWN_F64,
        }
    }
}

/// Everything live about one agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentStatus {
    /// Seconds since the agent process started.
    pub agent_uptime_secs: u64,
    /// Unix time of this sample.
    pub sampled_at: u64,
    /// Capacity state of the engine.
    pub state: ServingState,
    /// Requests crossing this agent right now; known even when the engine exposes no metrics.
    pub inflight_requests: u32,
    /// The inference engine.
    pub engine: EngineStatus,
    /// The host machine.
    pub host: HostStatus,
    /// One entry per GPU the agent can read; empty when it cannot read any.
    pub gpus: Vec<GpuStatus>,
    /// The agent's own speed test.
    pub benchmark: Benchmark,
}

impl AgentStatus {
    /// The state before the first sample.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            agent_uptime_secs: 0,
            sampled_at: 0,
            state: ServingState::Offline,
            inflight_requests: 0,
            engine: EngineStatus {
                kind: EngineKind::Unknown,
                version: None,
                available: false,
                metrics_enabled: false,
                max_slots: UNKNOWN,
                running_requests: UNKNOWN,
                queued_requests: UNKNOWN,
                tokens_per_second: UNKNOWN_F64,
                hint: None,
            },
            host: HostStatus {
                uptime_secs: 0,
                cpu_load_percent: UNKNOWN,
                memory_used_percent: UNKNOWN,
                cpu_temperature_c: UNKNOWN,
            },
            gpus: Vec::new(),
            benchmark: Benchmark::unknown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unreachable_engine_is_offline_whatever_it_last_reported() {
        assert_eq!(
            ServingState::from_load(false, 3, 0, 4),
            ServingState::Offline
        );
    }

    #[test]
    fn running_below_capacity_is_busy_and_at_capacity_is_occupied() {
        assert_eq!(ServingState::from_load(true, 0, 0, 4), ServingState::Idle);
        assert_eq!(ServingState::from_load(true, 3, 0, 4), ServingState::Busy);
        assert_eq!(
            ServingState::from_load(true, 4, 0, 4),
            ServingState::Occupied
        );
    }

    #[test]
    fn a_queue_means_occupied_even_when_the_capacity_is_unknown() {
        assert_eq!(
            ServingState::from_load(true, 2, 1, UNKNOWN),
            ServingState::Occupied
        );
        assert_eq!(
            ServingState::from_load(true, 2, 0, UNKNOWN),
            ServingState::Busy
        );
    }

    #[test]
    fn an_unread_load_is_not_mistaken_for_a_busy_engine() {
        assert_eq!(
            ServingState::from_load(true, UNKNOWN, UNKNOWN, 4),
            ServingState::Idle
        );
    }

    #[test]
    fn the_initial_status_round_trips_with_unknown_markers() {
        let json = serde_json::to_value(AgentStatus::initial()).unwrap();
        assert_eq!(json["engine"]["maxSlots"], -1);
        assert_eq!(json["host"]["cpuLoadPercent"], -1);
        assert_eq!(json["state"], "offline");
        assert_eq!(
            serde_json::from_value::<AgentStatus>(json).unwrap(),
            AgentStatus::initial()
        );
    }
}
