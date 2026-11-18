//! Identifiers that cross the trust boundary, validated once at parse time.

use std::fmt;

use thiserror::Error;

const MIN_CLIENT_ID_LEN: usize = 2;
const MAX_CLIENT_ID_LEN: usize = 63;
const MAX_MODEL_NAME_LEN: usize = 128;

/// LiteLLM parses `provider/model` prefixes, so a client id equal to a provider name could be
/// routed to that provider; the last three collide with relay paths.
const RESERVED_CLIENT_IDS: &[&str] = &[
    "anthropic",
    "azure",
    "azure_ai",
    "bedrock",
    "cloudflare",
    "cohere",
    "custom_openai",
    "databricks",
    "deepseek",
    "fireworks_ai",
    "gemini",
    "github",
    "google",
    "groq",
    "hosted_vllm",
    "huggingface",
    "jina_ai",
    "litellm",
    "litellm_proxy",
    "lm_studio",
    "mistral",
    "nvidia_nim",
    "ollama",
    "ollama_chat",
    "openai",
    "openai_like",
    "openrouter",
    "perplexity",
    "replicate",
    "sagemaker",
    "snowflake",
    "text-completion-openai",
    "together_ai",
    "vertex_ai",
    "vllm",
    "voyage",
    "watsonx",
    "xai",
    "healthz",
    "tunnel",
    "v1",
];

/// Why an identifier was rejected.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum IdError {
    /// The client id length is out of range.
    #[error("client id must be {MIN_CLIENT_ID_LEN} to {MAX_CLIENT_ID_LEN} characters")]
    ClientIdLength,
    /// The client id contains a forbidden character.
    #[error("client id must match [a-z0-9][a-z0-9_-]*")]
    ClientIdCharset,
    /// The client id collides with a name LiteLLM or the relay interprets.
    #[error("client id is reserved")]
    ClientIdReserved,
    /// The model name is empty or too long.
    #[error("model name must be 1 to {MAX_MODEL_NAME_LEN} characters")]
    ModelNameLength,
    /// The model name contains a forbidden character or path pattern.
    #[error("model name has a forbidden character or path pattern")]
    ModelNameCharset,
}

/// Namespace prefix of every model an agent serves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientId(String);

impl ClientId {
    /// Validates a raw client id.
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        if !(MIN_CLIENT_ID_LEN..=MAX_CLIENT_ID_LEN).contains(&raw.len()) {
            return Err(IdError::ClientIdLength);
        }
        let mut chars = raw.chars();
        let starts_ok = chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
        let rest_ok =
            chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
        if !starts_ok || !rest_ok {
            return Err(IdError::ClientIdCharset);
        }
        if RESERVED_CLIENT_IDS.contains(&raw) {
            return Err(IdError::ClientIdReserved);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The validated identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A model name exactly as the inference engine reports it.
///
/// It ends up in URLs and in LiteLLM model names, so the charset stays narrow while still
/// admitting Hugging Face style `org/model-7b` names.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModelName(String);

impl ModelName {
    /// Validates a raw model name.
    pub fn parse(raw: &str) -> Result<Self, IdError> {
        if raw.is_empty() || raw.len() > MAX_MODEL_NAME_LEN {
            return Err(IdError::ModelNameLength);
        }
        let charset_ok = raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '/' | '+'));
        let shape_ok = !raw.starts_with('/')
            && !raw.ends_with('/')
            && !raw.contains("//")
            && !raw.contains("..");
        if !charset_ok || !shape_ok {
            return Err(IdError::ModelNameCharset);
        }
        Ok(Self(raw.to_owned()))
    }

    /// The validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_client_ids() {
        for id in ["tester", "ab", "a1_b-2", "x".repeat(63).as_str()] {
            assert!(ClientId::parse(id).is_ok(), "{id}");
        }
    }

    #[test]
    fn rejects_bad_client_ids() {
        assert_eq!(ClientId::parse("a"), Err(IdError::ClientIdLength));
        assert_eq!(
            ClientId::parse(&"x".repeat(64)),
            Err(IdError::ClientIdLength)
        );
        assert_eq!(ClientId::parse("Tester"), Err(IdError::ClientIdCharset));
        assert_eq!(ClientId::parse("_tester"), Err(IdError::ClientIdCharset));
        assert_eq!(ClientId::parse("te/ster"), Err(IdError::ClientIdCharset));
        assert_eq!(ClientId::parse("te ster"), Err(IdError::ClientIdCharset));
    }

    #[test]
    fn rejects_reserved_client_ids() {
        for id in ["openai", "hosted_vllm", "healthz", "v1"] {
            assert_eq!(ClientId::parse(id), Err(IdError::ClientIdReserved), "{id}");
        }
    }

    #[test]
    fn accepts_engine_model_names() {
        for name in ["qwen3.8-27b", "Qwen/Qwen3-32B", "llama3:8b", "a+b_c.gguf"] {
            assert!(ModelName::parse(name).is_ok(), "{name}");
        }
    }

    #[test]
    fn rejects_unsafe_model_names() {
        for name in [
            "",
            "/models/x.gguf",
            "x/",
            "a//b",
            "../etc",
            "a b",
            "a?b",
            "a\nb",
            "é",
        ] {
            assert!(ModelName::parse(name).is_err(), "{name:?}");
        }
        assert_eq!(
            ModelName::parse(&"x".repeat(129)),
            Err(IdError::ModelNameLength)
        );
    }
}
