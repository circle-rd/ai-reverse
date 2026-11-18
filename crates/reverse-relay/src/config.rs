//! Typed relay configuration.

use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Url;
use reverse_core::config::{BaseUrl, ConfigError, Env};
use reverse_core::secret::SecretString;

const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_AUTH_SCOPE: &str = "tunnel:serve";

/// Everything the relay needs, validated at startup.
#[derive(Debug)]
pub struct Config {
    /// Where agents dial the tunnel protocol.
    pub tunnel_listen: SocketAddr,
    /// Where LiteLLM reaches the OpenAI-compatible endpoints.
    pub api_listen: SocketAddr,
    /// LiteLLM admin API, always with a trailing slash.
    pub litellm_url: Url,
    /// LiteLLM credential used to register models.
    pub litellm_master_key: SecretString,
    /// Origin of the tunnel listener as agents and the identity provider see it, without slash.
    pub public_url: String,
    /// Base LiteLLM uses to reach `api_listen`, without trailing slash; may carry a path prefix.
    pub public_api_base: String,
    /// Bearer key LiteLLM presents to `api_listen`.
    pub api_key: SecretString,
    /// Identity provider issuing the agents' access tokens.
    pub auth_issuer: String,
    /// Audience every tunnel token must carry; defaults to the public URL.
    pub auth_audience: String,
    /// Scope every tunnel token must carry.
    pub auth_scope: String,
    /// LiteLLM provider prefix of registered deployments.
    pub litellm_provider: String,
    /// Interval between polls of each agent's model list.
    pub models_poll: Duration,
    /// How long a vanished model stays registered.
    pub deregister_grace: Duration,
    /// How often the desired and actual model lists are compared.
    pub reconcile_interval: Duration,
    /// How often LiteLLM is re-read even when nothing changed.
    pub resync_interval: Duration,
    /// How long an agent may take to start answering a request.
    pub request_timeout: Duration,
    /// How long a body may stall in either direction.
    pub idle_timeout: Duration,
    /// Largest request body accepted from LiteLLM.
    pub max_request_bytes: usize,
    /// Bound on each LiteLLM admin call.
    pub litellm_timeout: Duration,
    /// Cap on simultaneously upgraded tunnels.
    pub max_tunnels: usize,
    /// Cap on simultaneously upgraded tunnels for one account.
    pub max_tunnels_per_user: usize,
    /// Failed tunnel authentications within the window that lock an address out.
    pub auth_failure_threshold: u32,
    /// Window that counts failed tunnel authentications.
    pub auth_failure_window: Duration,
    /// Lockout applied when the threshold is reached.
    pub auth_lockout: Duration,
    /// Whether the last `X-Forwarded-For` entry is the client address (a proxy sits in front).
    pub trust_forwarded_for: bool,
}

impl Config {
    /// Reads and validates the configuration.
    pub fn from_env<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<Self, ConfigError> {
        let litellm_url = parse_litellm_url(&env.required("RELAY_LITELLM_URL")?)?;
        let public = BaseUrl::parse("RELAY_PUBLIC_URL", &env.required("RELAY_PUBLIC_URL")?)?;
        let public_url = public.origin();
        let public_api_base = parse_api_base(&env.required("RELAY_PUBLIC_API_BASE")?)?;
        let models_poll = env.duration_secs("RELAY_MODELS_POLL_SECS", 5)?;
        let deregister_grace = env.duration_secs("RELAY_DEREGISTER_GRACE_SECS", 30)?;
        if deregister_grace < models_poll * 2 {
            return Err(ConfigError::invalid(
                "RELAY_DEREGISTER_GRACE_SECS",
                "must be at least twice RELAY_MODELS_POLL_SECS",
            ));
        }
        let auth_audience = match env.optional("RELAY_AUTH_AUDIENCE")? {
            Some(raw) => parse_https_url("RELAY_AUTH_AUDIENCE", &raw)?,
            None => public_url.clone(),
        };
        let auth_scope = env
            .optional("RELAY_AUTH_SCOPE")?
            .unwrap_or_else(|| DEFAULT_AUTH_SCOPE.to_owned());
        if auth_scope.is_empty()
            || !auth_scope
                .bytes()
                .all(|b| b == b' ' || b.is_ascii_graphic())
        {
            return Err(ConfigError::invalid(
                "RELAY_AUTH_SCOPE",
                "expected one or more scope tokens",
            ));
        }

        Ok(Self {
            tunnel_listen: socket_addr(env, "RELAY_TUNNEL_LISTEN")?,
            api_listen: socket_addr(env, "RELAY_API_LISTEN")?,
            litellm_url,
            litellm_master_key: env.required_secret("RELAY_LITELLM_MASTER_KEY")?,
            api_key: env.required_secret("RELAY_API_KEY")?,
            public_url,
            public_api_base,
            auth_issuer: parse_https_url("RELAY_AUTH_ISSUER", &env.required("RELAY_AUTH_ISSUER")?)?,
            auth_audience,
            auth_scope,
            litellm_provider: provider(env)?,
            models_poll,
            deregister_grace,
            reconcile_interval: env.duration_secs("RELAY_RECONCILE_INTERVAL_SECS", 2)?,
            resync_interval: env.duration_secs("RELAY_RESYNC_INTERVAL_SECS", 60)?,
            request_timeout: env.duration_secs("RELAY_REQUEST_HEADERS_TIMEOUT_SECS", 900)?,
            idle_timeout: env.duration_secs("RELAY_IDLE_TIMEOUT_SECS", 600)?,
            max_request_bytes: env.bytes("RELAY_MAX_REQUEST_BYTES", DEFAULT_MAX_REQUEST_BYTES)?,
            litellm_timeout: env.duration_secs("RELAY_LITELLM_TIMEOUT_SECS", 15)?,
            max_tunnels: env.bytes("RELAY_MAX_TUNNELS", 256)?,
            max_tunnels_per_user: env.bytes("RELAY_MAX_TUNNELS_PER_USER", 64)?,
            auth_failure_threshold: u32::try_from(env.bytes("RELAY_AUTH_FAILURE_THRESHOLD", 5)?)
                .map_err(|_| {
                    ConfigError::invalid(
                        "RELAY_AUTH_FAILURE_THRESHOLD",
                        "must fit in a 32-bit counter",
                    )
                })?,
            auth_failure_window: env.duration_secs("RELAY_AUTH_FAILURE_WINDOW_SECS", 60)?,
            auth_lockout: env.duration_secs("RELAY_AUTH_LOCKOUT_SECS", 60)?,
            trust_forwarded_for: env.flag("RELAY_TRUST_FORWARDED_FOR")?,
        })
    }
}

fn socket_addr<F: Fn(&str) -> Option<String>>(
    env: &Env<F>,
    key: &str,
) -> Result<SocketAddr, ConfigError> {
    env.required(key)?
        .trim()
        .parse()
        .map_err(|_| ConfigError::invalid(key, "expected ip:port"))
}

fn parse_api_base(raw: &str) -> Result<String, ConfigError> {
    let key = "RELAY_PUBLIC_API_BASE";
    let url = Url::parse(raw.trim()).map_err(|_| ConfigError::invalid(key, "not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::invalid(
            key,
            "expected an http(s) URL with a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err(ConfigError::invalid(
            key,
            "credentials and query are not allowed",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

/// The identity provider must be reached over TLS, except on loopback for local development.
fn parse_https_url(key: &str, raw: &str) -> Result<String, ConfigError> {
    let url = Url::parse(raw.trim()).map_err(|_| ConfigError::invalid(key, "not a valid URL"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    let acceptable = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !acceptable
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.query().is_some()
    {
        return Err(ConfigError::invalid(
            key,
            "expected an https URL without credentials or query",
        ));
    }
    Ok(url.as_str().trim_end_matches('/').to_owned())
}

fn parse_litellm_url(raw: &str) -> Result<Url, ConfigError> {
    let key = "RELAY_LITELLM_URL";
    let mut url =
        Url::parse(raw.trim()).map_err(|_| ConfigError::invalid(key, "not a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(ConfigError::invalid(
            key,
            "expected an http(s) URL with a host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err(ConfigError::invalid(
            key,
            "credentials and query are not allowed",
        ));
    }
    if !url.path().ends_with('/') {
        let with_slash = format!("{}/", url.path());
        url.set_path(&with_slash);
    }
    Ok(url)
}

fn provider<F: Fn(&str) -> Option<String>>(env: &Env<F>) -> Result<String, ConfigError> {
    let key = "RELAY_LITELLM_PROVIDER";
    let value = env
        .optional(key)?
        .unwrap_or_else(|| "hosted_vllm".to_owned());
    let valid = value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !valid {
        return Err(ConfigError::invalid(
            key,
            "expected a LiteLLM provider name like hosted_vllm",
        ));
    }
    Ok(value)
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

    const BASE: [(&str, &str); 8] = [
        ("RELAY_TUNNEL_LISTEN", "127.0.0.1:8443"),
        ("RELAY_API_LISTEN", "127.0.0.1:8088"),
        ("RELAY_LITELLM_URL", "http://litellm:4000"),
        ("RELAY_LITELLM_MASTER_KEY", "sk-test"),
        ("RELAY_PUBLIC_URL", "https://broker.example.com"),
        ("RELAY_PUBLIC_API_BASE", "https://broker.example.com/m/"),
        ("RELAY_API_KEY", "api-key"),
        ("RELAY_AUTH_ISSUER", "https://auth.example.com"),
    ];

    #[test]
    fn loads_with_defaults() {
        let cfg = load(&BASE).unwrap();
        assert_eq!(cfg.litellm_url.as_str(), "http://litellm:4000/");
        assert_eq!(cfg.public_api_base, "https://broker.example.com/m");
        assert_eq!(cfg.public_url, "https://broker.example.com");
        assert_eq!(cfg.auth_audience, "https://broker.example.com");
        assert_eq!(cfg.auth_scope, "tunnel:serve");
        assert_eq!(cfg.litellm_provider, "hosted_vllm");
        assert_eq!(cfg.max_tunnels_per_user, 64);
        assert_eq!(cfg.auth_failure_threshold, 5);
        assert_eq!(cfg.auth_failure_window, Duration::from_secs(60));
        assert_eq!(cfg.auth_lockout, Duration::from_secs(60));
    }

    #[test]
    fn every_required_setting_is_required() {
        for key in [
            "RELAY_API_KEY",
            "RELAY_AUTH_ISSUER",
            "RELAY_LITELLM_MASTER_KEY",
            "RELAY_PUBLIC_URL",
        ] {
            let pairs: Vec<_> = BASE.iter().filter(|(k, _)| *k != key).copied().collect();
            assert!(load(&pairs).is_err(), "{key}");
        }
    }

    #[test]
    fn the_identity_provider_needs_tls_off_loopback() {
        let mut pairs = BASE.to_vec();
        pairs[7] = ("RELAY_AUTH_ISSUER", "http://auth.example.com");
        assert!(load(&pairs).is_err());
        pairs[7] = ("RELAY_AUTH_ISSUER", "http://127.0.0.1:9000");
        assert!(load(&pairs).is_ok());
    }

    #[test]
    fn the_audience_and_scope_can_be_overridden() {
        let mut pairs = BASE.to_vec();
        pairs.extend([
            ("RELAY_AUTH_AUDIENCE", "https://relay.example.com"),
            ("RELAY_AUTH_SCOPE", "tunnel:serve"),
        ]);
        let cfg = load(&pairs).unwrap();
        assert_eq!(cfg.auth_audience, "https://relay.example.com");
        assert!(
            load(&[
                BASE[0],
                BASE[1],
                BASE[2],
                BASE[3],
                BASE[4],
                BASE[5],
                BASE[6],
                BASE[7],
                ("RELAY_AUTH_SCOPE", "bad scope\n")
            ])
            .is_err()
        );
    }

    #[test]
    fn grace_must_cover_two_polls() {
        let mut pairs = BASE.to_vec();
        pairs.push(("RELAY_MODELS_POLL_SECS", "20"));
        pairs.push(("RELAY_DEREGISTER_GRACE_SECS", "30"));
        assert!(load(&pairs).is_err());
    }

    #[test]
    fn rejects_bad_addresses_and_providers() {
        let mut pairs = BASE.to_vec();
        pairs[0] = ("RELAY_TUNNEL_LISTEN", "localhost");
        assert!(load(&pairs).is_err());
        let mut pairs = BASE.to_vec();
        pairs.push(("RELAY_LITELLM_PROVIDER", "Hosted VLLM"));
        assert!(load(&pairs).is_err());
    }

    #[test]
    fn litellm_url_keeps_a_path_prefix_with_trailing_slash() {
        let mut pairs = BASE.to_vec();
        pairs[2] = ("RELAY_LITELLM_URL", "https://gw.example.com/litellm");
        assert_eq!(
            load(&pairs).unwrap().litellm_url.as_str(),
            "https://gw.example.com/litellm/"
        );
    }
}
