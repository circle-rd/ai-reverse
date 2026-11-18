//! Typed agent configuration.

use std::path::PathBuf;
use std::time::Duration;

use reverse_core::config::{BaseUrl, ConfigError, Env};
use reverse_core::secret::SecretString;

const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_CREDENTIAL_FILE: &str = "/var/lib/reverse-agent/credential";

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
        })
    }
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
}
