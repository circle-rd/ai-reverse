//! LiteLLM management API client (`/model/*`).

use std::collections::HashSet;

use reqwest::{Client, Method, RequestBuilder, Url, redirect};
use reverse_core::ids::{ClientId, ModelName};
use reverse_core::secret::SecretString;
use reverse_core::wire::{AuthorSafetyClaim, HardwareInfo, ModelMetadata, ModelTag};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::LiteLlmError;

/// Tag marking deployments owned by this relay.
pub const MANAGED_BY: &str = "reverse-relay";

/// A deployment this relay created earlier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Managed {
    /// LiteLLM's deployment id.
    pub id: String,
    /// `{client_id}/{model}` as registered, if recorded.
    pub reverse_name: Option<String>,
    /// Account that owns the model, recorded in `model_info.created_by`.
    pub created_by: Option<String>,
    /// Capability tags recorded in `model_info`, as sent.
    pub tags: Vec<String>,
    pub hub_owner: Option<String>,
    pub hub_avatar_url: Option<String>,
    pub author_safety_claim: Option<AuthorSafetyClaim>,
    /// Routing mode recorded in `model_info`.
    pub mode: Option<String>,
    /// Capability flags recorded in `model_info`.
    pub supports: Supports,
    /// Hardware of the nodes serving the model, as recorded in `model_info`; empty when absent.
    pub hardware: Vec<HardwareInfo>,
}

/// Capability flags this relay publishes in `model_info`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Supports {
    pub function_calling: bool,
    pub vision: bool,
    pub video_input: bool,
    pub audio_input: bool,
    pub audio_output: bool,
}

/// The LiteLLM fields derived from the announced capability tags.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Publish {
    pub mode: Option<&'static str>,
    pub supports: Supports,
}

impl Publish {
    pub(crate) fn from_tags(tags: &[ModelTag]) -> Self {
        let has = |tag| tags.contains(&tag);
        // LiteLLM's mode names the endpoint family a deployment serves. One speech lane answers
        // both recognition and synthesis, so it keeps the chat surface and its audio flags.
        let mode = if has(ModelTag::Embedding) {
            Some("embedding")
        } else if has(ModelTag::Image) {
            Some("image_generation")
        } else if has(ModelTag::Transcription) {
            Some("audio_transcription")
        } else if has(ModelTag::Video) {
            Some("video_generation")
        } else {
            None
        };
        Self {
            mode,
            supports: Supports {
                function_calling: has(ModelTag::Tools),
                vision: has(ModelTag::Vision),
                video_input: has(ModelTag::Video),
                audio_input: has(ModelTag::Speech) || has(ModelTag::Transcription),
                audio_output: has(ModelTag::Speech),
            },
        }
    }
}

#[derive(Deserialize)]
struct ModelInfoList {
    data: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    model_info: Option<RowInfo>,
}

/// The deployments this relay owns, one per deployment id.
fn managed_rows(list: ModelInfoList) -> Vec<Managed> {
    let mut seen = HashSet::new();
    list.data
        .into_iter()
        .filter_map(|row| row.model_info)
        .filter(|info| info.managed_by.as_deref() == Some(MANAGED_BY))
        .filter_map(|info| {
            let id = info.id?;
            seen.insert(id.clone()).then_some(Managed {
                id,
                reverse_name: info.reverse_name,
                created_by: info.created_by,
                tags: info.tags,
                hub_owner: info.hub_owner,
                hub_avatar_url: info.hub_avatar_url,
                author_safety_claim: info.author_safety_claim,
                mode: info.mode,
                supports: Supports {
                    function_calling: info.supports_function_calling,
                    vision: info.supports_vision,
                    video_input: info.supports_video_input,
                    audio_input: info.supports_audio_input,
                    audio_output: info.supports_audio_output,
                },
                hardware: info.hardware.unwrap_or_default(),
            })
        })
        .collect()
}

/// LiteLLM persists a capability flag we omit as JSON `null` rather than dropping
/// the key, so a flag is accepted either as a boolean or as null.
fn null_to_false<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Ok(Option::<bool>::deserialize(deserializer)?.unwrap_or(false))
}

#[derive(Deserialize)]
struct RowInfo {
    id: Option<String>,
    managed_by: Option<String>,
    reverse_name: Option<String>,
    created_by: Option<String>,
    mode: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    hardware: Option<Vec<HardwareInfo>>,
    #[serde(default)]
    hub_owner: Option<String>,
    #[serde(default)]
    hub_avatar_url: Option<String>,
    #[serde(default)]
    author_safety_claim: Option<AuthorSafetyClaim>,
    #[serde(default, deserialize_with = "null_to_false")]
    supports_function_calling: bool,
    #[serde(default, deserialize_with = "null_to_false")]
    supports_vision: bool,
    #[serde(default, deserialize_with = "null_to_false")]
    supports_video_input: bool,
    #[serde(default, deserialize_with = "null_to_false")]
    supports_audio_input: bool,
    #[serde(default, deserialize_with = "null_to_false")]
    supports_audio_output: bool,
}

#[derive(Serialize)]
struct NewModel<'a> {
    model_name: String,
    litellm_params: LiteLlmParams<'a>,
    model_info: NewModelInfo<'a>,
}

#[derive(Serialize)]
struct LiteLlmParams<'a> {
    model: String,
    api_base: String,
    api_key: &'a str,
}

#[derive(Serialize)]
struct NewModelInfo<'a> {
    managed_by: &'static str,
    client_id: &'a str,
    created_by: &'a str,
    reverse_name: String,
    tags: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub_owner: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hub_avatar_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    author_safety_claim: Option<AuthorSafetyClaim>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hardware: Option<&'a [HardwareInfo]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_function_calling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_video_input: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_audio_input: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    supports_audio_output: Option<bool>,
}

#[derive(Serialize)]
struct DeleteModel<'a> {
    id: &'a str,
}

/// Registers and removes deployments in LiteLLM.
#[derive(Clone)]
pub struct LiteLlm {
    http: Client,
    base: Url,
    key: SecretString,
    api_key: SecretString,
    provider: String,
    public_api_base: String,
}

impl LiteLlm {
    /// Builds a client from the validated configuration.
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        let http = Client::builder()
            .timeout(config.litellm_timeout)
            .connect_timeout(config.litellm_timeout)
            .redirect(redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            base: config.litellm_url.clone(),
            key: config.litellm_management_key.clone(),
            api_key: config.api_key.clone(),
            provider: config.litellm_provider.clone(),
            public_api_base: config.public_api_base.clone(),
        })
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, LiteLlmError> {
        let url = self
            .base
            .join(path)
            .map_err(|e| LiteLlmError::Url(e.to_string()))?;
        Ok(self
            .http
            .request(method, url)
            .bearer_auth(self.key.expose()))
    }

    async fn send(builder: RequestBuilder) -> Result<reqwest::Response, LiteLlmError> {
        let response = builder.send().await?;
        if response.status().is_success() {
            Ok(response)
        } else {
            Err(LiteLlmError::Status(response.status()))
        }
    }

    /// Lists the deployments owned by this relay, one entry per deployment id.
    pub async fn list_managed(&self) -> Result<Vec<Managed>, LiteLlmError> {
        let response = Self::send(self.request(Method::GET, "model/info")?).await?;
        let list: ModelInfoList = response.json().await?;
        Ok(managed_rows(list))
    }

    /// Registers `{client}/{model}` as an OpenAI-compatible deployment served by the relay.
    pub async fn create(
        &self,
        client: &ClientId,
        model: &ModelName,
        metadata: &ModelMetadata,
        hardware: &[HardwareInfo],
    ) -> Result<(), LiteLlmError> {
        let name = format!("{client}/{model}");
        let publish = Publish::from_tags(&metadata.tags);
        let body = NewModel {
            model_name: name.clone(),
            litellm_params: LiteLlmParams {
                model: format!("{}/{model}", self.provider),
                api_base: format!("{}/{client}/v1", self.public_api_base),
                api_key: self.api_key.expose(),
            },
            model_info: NewModelInfo {
                managed_by: MANAGED_BY,
                client_id: client.as_str(),
                created_by: client.as_str(),
                reverse_name: name,
                tags: metadata.tags.iter().map(|tag| tag.as_str()).collect(),
                hub_owner: metadata.hub_owner.as_deref(),
                hub_avatar_url: metadata.hub_avatar_url.as_deref(),
                author_safety_claim: metadata.author_safety_claim,
                hardware: (!hardware.is_empty()).then_some(hardware),
                mode: publish.mode,
                supports_function_calling: publish.supports.function_calling.then_some(true),
                supports_vision: publish.supports.vision.then_some(true),
                supports_video_input: publish.supports.video_input.then_some(true),
                supports_audio_input: publish.supports.audio_input.then_some(true),
                supports_audio_output: publish.supports.audio_output.then_some(true),
            },
        };
        Self::send(self.request(Method::POST, "model/new")?.json(&body)).await?;
        Ok(())
    }

    /// Removes a deployment by id.
    pub async fn delete(&self, id: &str) -> Result<(), LiteLlmError> {
        Self::send(
            self.request(Method::POST, "model/delete")?
                .json(&DeleteModel { id }),
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reverse_core::wire::HardwareKind;

    #[test]
    fn parses_flags_litellm_persisted_as_null() {
        // LiteLLM stores omitted capability flags as JSON null, not as absent keys.
        let list: ModelInfoList = serde_json::from_str(
            r#"{"data":[{"model_info":{"id":"1","managed_by":"reverse-relay","reverse_name":"bmz/m","created_by":"bmz","tags":["text"],"supports_function_calling":true,"supports_vision":null,"supports_video_input":null,"supports_audio_input":null,"supports_audio_output":null}}]}"#,
        )
        .expect("a row with null flags");
        let managed = managed_rows(list);
        assert_eq!(managed.len(), 1);
        assert!(managed[0].supports.function_calling);
        assert!(!managed[0].supports.vision);
        assert!(managed[0].hardware.is_empty());
    }

    #[test]
    fn reads_the_hardware_recorded_on_a_deployment() {
        let list: ModelInfoList = serde_json::from_str(
            r#"{"data":[{"model_info":{"id":"1","managed_by":"reverse-relay","reverse_name":"bmz/m","hardware":[{"kind":"dgx-spark","product":"NVIDIA DGX Spark","vendor":null,"gpus":["NVIDIA GB10"],"memoryMb":122880,"cpuArch":"aarch64","cpuCores":20}]}}]}"#,
        )
        .expect("a row with hardware");
        let managed = managed_rows(list);
        assert_eq!(managed[0].hardware.len(), 1);
        assert_eq!(managed[0].hardware[0].kind, HardwareKind::DgxSpark);
        assert_eq!(managed[0].hardware[0].cpu_cores, 20);
    }
}

#[cfg(test)]
mod publish_tests {
    use super::*;

    #[test]
    fn each_media_tag_names_its_litellm_endpoint_family() {
        assert_eq!(
            Publish::from_tags(&[ModelTag::Embedding]).mode,
            Some("embedding")
        );
        assert_eq!(
            Publish::from_tags(&[ModelTag::Image]).mode,
            Some("image_generation")
        );
        assert_eq!(
            Publish::from_tags(&[ModelTag::Transcription]).mode,
            Some("audio_transcription")
        );
        assert_eq!(
            Publish::from_tags(&[ModelTag::Video]).mode,
            Some("video_generation")
        );
        assert_eq!(
            Publish::from_tags(&[ModelTag::Text, ModelTag::Tools]).mode,
            None
        );
    }

    #[test]
    fn a_speech_model_keeps_the_chat_surface_and_its_audio_flags() {
        let speech = Publish::from_tags(&[ModelTag::Speech]);
        assert_eq!(speech.mode, None);
        assert!(speech.supports.audio_input && speech.supports.audio_output);
        let asr = Publish::from_tags(&[ModelTag::Transcription]);
        assert!(asr.supports.audio_input && !asr.supports.audio_output);
    }
}
