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

/// A capability tag used to classify an announced model.
///
/// The engine does not expose capabilities, so the agent combines operator declarations with
/// the hub's `pipeline_tag` metadata; models with neither carry no tags. Downstream consumers
/// receive these tags through the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelTag {
    /// Text generation.
    Text,
    /// Tool / function calling.
    Tools,
    /// Image understanding.
    Vision,
    /// Image generation.
    Image,
    /// Video generation or understanding.
    Video,
    /// Speech recognition or synthesis.
    Speech,
    /// Embedding vectors.
    Embedding,
    /// Reranking.
    Rerank,
}

impl ModelTag {
    /// Parses a tag written in kebab-case.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "text" => Some(Self::Text),
            "tools" => Some(Self::Tools),
            "vision" => Some(Self::Vision),
            "image" => Some(Self::Image),
            "video" => Some(Self::Video),
            "speech" => Some(Self::Speech),
            "embedding" => Some(Self::Embedding),
            "rerank" => Some(Self::Rerank),
            _ => None,
        }
    }

    /// The kebab-case spelling used on the wire and in configuration files.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Tools => "tools",
            Self::Vision => "vision",
            Self::Image => "image",
            Self::Video => "video",
            Self::Speech => "speech",
            Self::Embedding => "embedding",
            Self::Rerank => "rerank",
        }
    }
}

/// One model the agent announces to the relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelEntry {
    /// Model id exactly as the engine (or the operator) reports it.
    pub id: String,
    /// Capability tags, sorted and deduplicated.
    pub tags: Vec<ModelTag>,
}

/// What the inference engine serves right now, as seen by the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsSnapshot {
    /// Incremented on every change of `models` or `engine_available`.
    pub revision: u64,
    /// Whether the last poll of the engine succeeded.
    pub engine_available: bool,
    /// Models announced by the agent, including configured extras.
    pub models: Vec<ModelEntry>,
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
    fn a_model_entry_carries_kebab_case_tags() {
        let entry = ModelEntry {
            id: "perplexity-ai/pplx-embed-v1-0.6b".to_owned(),
            tags: vec![ModelTag::Embedding],
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert_eq!(
            json,
            r#"{"id":"perplexity-ai/pplx-embed-v1-0.6b","tags":["embedding"]}"#
        );
        assert_eq!(serde_json::from_str::<ModelEntry>(&json).unwrap(), entry);
    }

    #[test]
    fn tags_round_trip_through_their_wire_spelling() {
        for tag in [
            ModelTag::Text,
            ModelTag::Tools,
            ModelTag::Vision,
            ModelTag::Image,
            ModelTag::Video,
            ModelTag::Speech,
            ModelTag::Embedding,
            ModelTag::Rerank,
        ] {
            assert_eq!(ModelTag::parse(tag.as_str()), Some(tag));
        }
        assert_eq!(ModelTag::parse("embeddings"), None);
    }

    #[test]
    fn serde_and_the_config_spelling_cannot_drift() {
        for tag in [
            ModelTag::Text,
            ModelTag::Tools,
            ModelTag::Vision,
            ModelTag::Image,
            ModelTag::Video,
            ModelTag::Speech,
            ModelTag::Embedding,
            ModelTag::Rerank,
        ] {
            let json = serde_json::to_string(&tag).unwrap();
            assert_eq!(json, format!("\"{}\"", tag.as_str()));
            let parsed: ModelTag = serde_json::from_str(&json).unwrap();
            assert_eq!(ModelTag::parse(parsed.as_str()), Some(parsed));
        }
    }

    #[test]
    fn agent_info_round_trips_with_camel_case_and_kebab_kind() {
        let json = r#"{"agentVersion":"0.1.0","startedAt":5,"hardware":{"kind":"dgx-spark","product":null,"vendor":null,"gpus":["NVIDIA GB10"],"memoryMb":122880,"cpuArch":"aarch64","cpuCores":20}}"#;
        let info: AgentInfo = serde_json::from_str(json).unwrap();
        assert_eq!(info.hardware.kind, HardwareKind::DgxSpark);
        assert_eq!(serde_json::to_string(&info).unwrap(), json);
    }
}
