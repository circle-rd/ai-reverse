//! LiteLLM management API client (`/model/*`).

use std::collections::HashSet;

use reqwest::{Client, Method, RequestBuilder, Url, redirect};
use reverse_core::ids::{ClientId, ModelName};
use reverse_core::secret::SecretString;
use reverse_core::wire::ModelTag;
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
    /// Capability tags recorded in `model_info`, as sent.
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
struct ModelInfoList {
    data: Vec<Row>,
}

#[derive(Deserialize)]
struct Row {
    model_info: Option<RowInfo>,
}

#[derive(Deserialize)]
struct RowInfo {
    id: Option<String>,
    managed_by: Option<String>,
    reverse_name: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
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
    reverse_name: String,
    tags: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<&'static str>,
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
        let mut seen = HashSet::new();
        Ok(list
            .data
            .into_iter()
            .filter_map(|row| row.model_info)
            .filter(|info| info.managed_by.as_deref() == Some(MANAGED_BY))
            .filter_map(|info| {
                let id = info.id?;
                seen.insert(id.clone()).then_some(Managed {
                    id,
                    reverse_name: info.reverse_name,
                    tags: info.tags,
                })
            })
            .collect())
    }

    /// Registers `{client}/{model}` as an OpenAI-compatible deployment served by the relay.
    pub async fn create(
        &self,
        client: &ClientId,
        model: &ModelName,
        tags: &[ModelTag],
    ) -> Result<(), LiteLlmError> {
        let name = format!("{client}/{model}");
        // LiteLLM reads `mode` from `model_info` to route embeddings correctly.
        let mode = tags.contains(&ModelTag::Embedding).then_some("embedding");
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
                reverse_name: name,
                tags: tags.iter().map(|tag| tag.as_str()).collect(),
                mode,
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
