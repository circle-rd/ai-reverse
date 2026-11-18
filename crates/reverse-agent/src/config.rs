//! Typed agent configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use reverse_core::config::{BaseUrl, ConfigError, Env};
use reverse_core::ids::ModelName;
use reverse_core::secret::SecretString;
use reverse_core::wire::ModelTag;

const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_CREDENTIAL_FILE: &str = "/var/lib/reverse-agent/credential";
const DEFAULT_HF_URL: &str = "https://huggingface.co";

/// Everything the agent needs, validated at startup.
#[derive(Debug)]
pub struct Config {
    /// Where to dial the tunnel.
    pub relay: BaseUrl,
    /// A pre-provisioned credential; when absent the agent pairs with the relay.
    pub token: Option<SecretString>,
    /// Where the credential obtained by pairing is kept.
    pub credential_file: PathBuf,
    /// Name shown to the person approving the pairing.
    pub label: String,
    /// The local OpenAI-compatible engine.
    pub upstream: BaseUrl,
    /// Bearer key the engine requires, if any.
    pub upstream_api_key: Option<SecretString>,
    /// Interval between polls of the engine's model list.
    pub models_poll: Duration,
    /// First reconnect delay.
    pub reconnect_min: Duration,
    /// Reconnect delay ceiling.
    pub reconnect_max: Duration,
    /// Bound on TCP connect and on the tunnel upgrade.
    pub connect_timeout: Duration,
    /// How long the engine may take to start answering a request.
    pub upstream_headers_timeout: Duration,
    /// How long a body may stall in either direction.
    pub idle_timeout: Duration,
    /// Largest request body accepted from the relay.
    pub max_request_bytes: usize,
    /// Models to announce even though the engine's `/v1/models` omits them.
    pub extra_models: Vec<ModelName>,
    /// Operator-declared capability tags, replacing inference for the named models.
    pub model_tags: BTreeMap<ModelName, Vec<ModelTag>>,
    /// Hugging Face hub origin used to classify models.
    pub hf_url: BaseUrl,
    /// Token for gated or private repositories on the hub.
    pub hf_token: Option<SecretString>,
    /// Bound on one metadata lookup.
    pub hf_timeout: Duration,
    /// Whether the hub is consulted at all; `off` keeps model ids on the box.
    pub hf_enabled: bool,
}

impl Config {
    /// Reads and validates the configuration.
    pub fn from_env<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<Self, ConfigError> {
        let relay = BaseUrl::parse("AGENT_RELAY_URL", &env.required("AGENT_RELAY_URL")?)?;
        let upstream = BaseUrl::parse("AGENT_UPSTREAM_URL", &env.required("AGENT_UPSTREAM_URL")?)?;

        let insecure = env.flag("AGENT_INSECURE_TRANSPORT")?;
        for (key, url) in [
            ("AGENT_RELAY_URL", &relay),
            ("AGENT_UPSTREAM_URL", &upstream),
        ] {
            let tls = url.scheme() == &http::uri::Scheme::HTTPS;
            if key == "AGENT_UPSTREAM_URL" && tls {
                return Err(ConfigError::invalid(
                    key,
                    "TLS to the local engine is not supported, use http",
                ));
            }
            if !tls && !url.is_loopback() && !insecure {
                return Err(ConfigError::invalid(
                    key,
                    "plain-text transport to a non-loopback host requires AGENT_INSECURE_TRANSPORT=true",
                ));
            }
        }

        let reconnect_min = env.duration_secs("AGENT_RECONNECT_MIN_SECS", 1)?;
        let reconnect_max = env.duration_secs("AGENT_RECONNECT_MAX_SECS", 30)?;
        if reconnect_min > reconnect_max {
            return Err(ConfigError::invalid(
                "AGENT_RECONNECT_MIN_SECS",
                "must not exceed AGENT_RECONNECT_MAX_SECS",
            ));
        }

        let hf_enabled = match env.optional("AGENT_HF_METADATA")?.as_deref() {
            None | Some("on") | Some("true") => true,
            Some("off") | Some("false") => false,
            Some(_) => {
                return Err(ConfigError::invalid(
                    "AGENT_HF_METADATA",
                    "expected on/off (true/false)",
                ));
            }
        };
        let hf_url = BaseUrl::parse(
            "AGENT_HF_URL",
            &env.optional("AGENT_HF_URL")?
                .unwrap_or_else(|| DEFAULT_HF_URL.to_owned()),
        )?;
        if hf_url.scheme() != &http::uri::Scheme::HTTPS && !hf_url.is_loopback() {
            return Err(ConfigError::invalid(
                "AGENT_HF_URL",
                "must use https unless it points at a loopback address",
            ));
        }

        Ok(Self {
            relay,
            token: env.secret("AGENT_TOKEN")?,
            credential_file: PathBuf::from(
                env.optional("AGENT_CREDENTIAL_FILE")?
                    .unwrap_or_else(|| DEFAULT_CREDENTIAL_FILE.to_owned()),
            ),
            label: env
                .optional("AGENT_LABEL")?
                .unwrap_or_else(|| "agent".to_owned()),
            upstream,
            upstream_api_key: env.secret("AGENT_UPSTREAM_API_KEY")?,
            models_poll: env.duration_secs("AGENT_MODELS_POLL_SECS", 5)?,
            reconnect_min,
            reconnect_max,
            connect_timeout: env.duration_secs("AGENT_CONNECT_TIMEOUT_SECS", 5)?,
            upstream_headers_timeout: env
                .duration_secs("AGENT_UPSTREAM_HEADERS_TIMEOUT_SECS", 900)?,
            idle_timeout: env.duration_secs("AGENT_IDLE_TIMEOUT_SECS", 600)?,
            max_request_bytes: env.bytes("AGENT_MAX_REQUEST_BYTES", DEFAULT_MAX_REQUEST_BYTES)?,
            extra_models: parse_extra_models(env)?,
            model_tags: parse_model_tags(env)?,
            hf_url,
            hf_token: env.secret("AGENT_HF_TOKEN")?,
            hf_timeout: env.duration_secs("AGENT_HF_TIMEOUT_SECS", 5)?,
            hf_enabled,
        })
    }
}

/// Reads `AGENT_EXTRA_MODELS`: comma-separated model ids announced in addition to discovery.
fn parse_extra_models<F: Fn(&str) -> Option<String>>(
    env: &Env<F>,
) -> Result<Vec<ModelName>, ConfigError> {
    const KEY: &str = "AGENT_EXTRA_MODELS";
    let Some(raw) = env.optional(KEY)? else {
        return Ok(Vec::new());
    };
    let mut models = Vec::new();
    for part in raw.split(',') {
        let id = part.trim();
        if id.is_empty() {
            return Err(ConfigError::invalid(KEY, "empty model id"));
        }
        let name =
            ModelName::parse(id).map_err(|error| ConfigError::invalid(KEY, error.to_string()))?;
        if models.contains(&name) {
            return Err(ConfigError::invalid(KEY, "duplicate model id"));
        }
        models.push(name);
    }
    Ok(models)
}

/// Reads `AGENT_MODEL_TAGS`: `model=tag[,tag][;model=tag...]`, validated at startup.
fn parse_model_tags<F: Fn(&str) -> Option<String>>(
    env: &Env<F>,
) -> Result<BTreeMap<ModelName, Vec<ModelTag>>, ConfigError> {
    const KEY: &str = "AGENT_MODEL_TAGS";
    let Some(raw) = env.optional(KEY)? else {
        return Ok(BTreeMap::new());
    };
    let mut map = BTreeMap::new();
    for entry in raw.split(';') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(ConfigError::invalid(KEY, "empty entry"));
        }
        let Some((id, tags)) = entry.split_once('=') else {
            return Err(ConfigError::invalid(KEY, "expected model=tag[,tag]"));
        };
        let name = ModelName::parse(id.trim())
            .map_err(|error| ConfigError::invalid(KEY, error.to_string()))?;
        let mut parsed = Vec::new();
        for tag in tags.split(',') {
            let tag = tag.trim();
            if tag.is_empty() {
                return Err(ConfigError::invalid(KEY, "empty tag"));
            }
            let tag = ModelTag::parse(tag)
                .ok_or_else(|| ConfigError::invalid(KEY, format!("unknown tag {tag}")))?;
            if !parsed.contains(&tag) {
                parsed.push(tag);
            }
        }
        parsed.sort();
        if map.insert(name, parsed).is_some() {
            return Err(ConfigError::invalid(KEY, "duplicate model id"));
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Config::from_env(&Env::new(move |key| map.get(key).cloned()))
    }

    const BASE: [(&str, &str); 2] = [
        ("AGENT_RELAY_URL", "http://127.0.0.1:8443"),
        ("AGENT_UPSTREAM_URL", "http://127.0.0.1:30001"),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
        let mut all = BASE.to_vec();
        all.extend_from_slice(extra);
        all
    }

    #[test]
    fn loads_a_minimal_loopback_setup() {
        let cfg = load(&BASE).unwrap();
        assert!(cfg.token.is_none());
        assert!(cfg.upstream_api_key.is_none());
    }

    #[test]
    fn a_static_token_comes_from_the_environment() {
        let cfg = load(&with(&[("AGENT_TOKEN", "rvr_x")])).unwrap();
        assert_eq!(cfg.token.unwrap().expose(), "rvr_x");
    }

    #[test]
    fn the_relay_url_is_required() {
        assert!(load(&BASE[1..]).is_err());
    }

    #[test]
    fn plain_text_to_a_remote_relay_needs_an_explicit_flag() {
        let mut pairs = vec![BASE[1], ("AGENT_RELAY_URL", "http://relay.lan:8443")];
        assert!(load(&pairs).is_err());
        pairs.push(("AGENT_INSECURE_TRANSPORT", "true"));
        assert!(load(&pairs).is_ok());
    }

    #[test]
    fn tls_is_accepted_for_the_relay_but_not_the_engine() {
        let relay = [BASE[1], ("AGENT_RELAY_URL", "https://broker.example.com")];
        assert!(load(&relay).is_ok());
        let engine = [BASE[0], ("AGENT_UPSTREAM_URL", "https://127.0.0.1:30001")];
        assert!(load(&engine).is_err());
    }

    #[test]
    fn reconnect_bounds_are_ordered() {
        let pairs = with(&[
            ("AGENT_RECONNECT_MIN_SECS", "60"),
            ("AGENT_RECONNECT_MAX_SECS", "30"),
        ]);
        assert!(load(&pairs).is_err());
    }

    #[test]
    fn upstream_key_comes_from_the_environment_or_a_file() {
        let cfg = load(&with(&[("AGENT_UPSTREAM_API_KEY", "k")])).unwrap();
        assert_eq!(cfg.upstream_api_key.unwrap().expose(), "k");
    }

    #[test]
    fn extra_models_and_tags_are_parsed_and_sorted() {
        let cfg = load(&with(&[
            ("AGENT_EXTRA_MODELS", "perplexity-ai/pplx-embed-v1-0.6b"),
            ("AGENT_MODEL_TAGS", "qwen3.8-flash-next=tools,text"),
        ]))
        .unwrap();
        assert_eq!(cfg.extra_models.len(), 1);
        assert_eq!(
            cfg.extra_models[0].as_str(),
            "perplexity-ai/pplx-embed-v1-0.6b"
        );
        let key = ModelName::parse("qwen3.8-flash-next").unwrap();
        assert_eq!(
            cfg.model_tags.get(&key),
            Some(&vec![ModelTag::Text, ModelTag::Tools])
        );
    }

    #[test]
    fn invalid_model_declarations_are_rejected() {
        assert!(load(&with(&[("AGENT_MODEL_TAGS", "m=embeddings")])).is_err());
        assert!(load(&with(&[("AGENT_MODEL_TAGS", "m=text;m=text")])).is_err());
        assert!(load(&with(&[("AGENT_MODEL_TAGS", "m")])).is_err());
        assert!(load(&with(&[("AGENT_EXTRA_MODELS", "a,a")])).is_err());
        assert!(load(&with(&[("AGENT_EXTRA_MODELS", "/bad")])).is_err());
    }

    #[test]
    fn the_hub_is_on_by_default_and_can_be_disabled() {
        assert!(load(&BASE).unwrap().hf_enabled);
        assert!(
            !load(&with(&[("AGENT_HF_METADATA", "off")]))
                .unwrap()
                .hf_enabled
        );
        assert!(
            !load(&with(&[("AGENT_HF_METADATA", "false")]))
                .unwrap()
                .hf_enabled
        );
        assert!(
            load(&with(&[("AGENT_HF_METADATA", "true")]))
                .unwrap()
                .hf_enabled
        );
        assert!(load(&with(&[("AGENT_HF_METADATA", "maybe")])).is_err());
        assert_eq!(
            load(&BASE).unwrap().hf_url.origin(),
            "https://huggingface.co"
        );
    }

    #[test]
    fn the_hub_origin_must_be_https_unless_loopback() {
        assert!(load(&with(&[("AGENT_HF_URL", "http://huggingface.co")])).is_err());
        assert!(load(&with(&[("AGENT_HF_URL", "http://127.0.0.1:9")])).is_ok());
    }

    #[test]
    fn the_hub_token_reads_the_file_form() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hf");
        std::fs::write(&path, "hf_secret\n").unwrap();
        let mut map: HashMap<String, String> = BASE
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        map.insert(
            "AGENT_HF_TOKEN_FILE".to_owned(),
            path.to_str().unwrap().to_owned(),
        );
        let cfg = Config::from_env(&Env::new(move |key| map.get(key).cloned())).unwrap();
        assert_eq!(cfg.hf_token.unwrap().expose(), "hf_secret");
    }
}
