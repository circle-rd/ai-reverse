//! Wire contract between the agent and the relay.

use serde::{Deserialize, Serialize};

/// Protocol token of the HTTP/1.1 upgrade; after it the relay is the HTTP/2 client.
pub const TUNNEL_PROTOCOL: &str = "reverse-h2/1";

/// Path the agent dials to open a tunnel.
pub const TUNNEL_PATH: &str = "/tunnel";

/// Path the relay polls through the tunnel to learn which models are served.
pub const AGENT_MODELS_PATH: &str = "/_agent/models";

/// Path the relay polls through the tunnel to learn what machine the agent runs on.
pub const AGENT_INFO_PATH: &str = "/_agent/info";

/// Authority used for requests sent through the tunnel; the connection already picks the peer.
pub const TUNNEL_AUTHORITY: &str = "reverse-agent";

/// The class of machine, as far as the agent can tell; only self-reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HardwareKind {
    /// NVIDIA DGX Spark or another GB10 machine.
    DgxSpark,
    /// Anything else.
    Other,
}

/// What the agent read about its host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HardwareInfo {
    /// The class of machine.
    pub kind: HardwareKind,
    /// DMI product name, e.g. `NVIDIA DGX Spark`.
    pub product: Option<String>,
    /// DMI system vendor.
    pub vendor: Option<String>,
    /// GPU model names reported by the NVIDIA driver.
    pub gpus: Vec<String>,
    /// Total memory in MiB (unified memory on GB10).
    pub memory_mb: u64,
    /// CPU architecture, e.g. `aarch64`.
    pub cpu_arch: String,
    /// Logical CPU count.
    pub cpu_cores: u32,
}

/// Static facts about one agent process, served at [`AGENT_INFO_PATH`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentInfo {
    /// Agent crate version.
    pub agent_version: String,
    /// Unix time the agent process started.
    pub started_at: u64,
    /// The host machine.
    pub hardware: HardwareInfo,
}

/// What the inference engine serves right now, as seen by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsSnapshot {
    /// Incremented on every change of `models` or `engine_available`.
    pub revision: u64,
    /// Whether the last poll of the engine succeeded.
    pub engine_available: bool,
    /// Model ids reported by the engine.
    pub models: Vec<String>,
}

impl ModelsSnapshot {
    /// The state before the first poll.
    #[must_use]
    pub fn initial() -> Self {
        Self {
            revision: 0,
            engine_available: false,
            models: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_uses_camel_case_on_the_wire() {
        let json = serde_json::to_string(&ModelsSnapshot::initial()).unwrap();
        assert_eq!(
            json,
            r#"{"revision":0,"engineAvailable":false,"models":[]}"#
        );
    }

    #[test]
    fn agent_info_round_trips_with_camel_case_and_kebab_kind() {
        let json = r#"{"agentVersion":"0.1.0","startedAt":5,"hardware":{"kind":"dgx-spark","product":null,"vendor":null,"gpus":["NVIDIA GB10"],"memoryMb":122880,"cpuArch":"aarch64","cpuCores":20}}"#;
        let info: AgentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.hardware.kind, HardwareKind::DgxSpark);
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
    }
}
