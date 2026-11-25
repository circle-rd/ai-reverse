//! Typed relay configuration.

use std::net::SocketAddr;
use std::time::Duration;

use reqwest::Url;
use reverse_core::config::{BaseUrl, ConfigError, Env};
use reverse_core::secret::SecretString;

const DEFAULT_MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_AUTH_SCOPE: &str = "tunnel:serve";

/// The client the relay authenticates as when it tells auth-service the models changed.
#[derive(Debug, Clone)]
pub struct CatalogueCredentials {
    /// Confidential client id registered in auth-service.
    pub client_id: String,
    /// Client secret, from `RELAY_CATALOGUE_CLIENT_SECRET` or its `_FILE` form.
    pub client_secret: SecretString,
}

/// Where the relay pushes its inventory and receives the owners' settings.
#[derive(Debug, Clone)]
pub struct InventoryConfig {
    /// The receiver's endpoint.
    pub url: String,
    /// Shared secret that signs each push.
    pub secret: SecretString,
    /// Names this relay in the receiver's records.
    pub relay_id: String,
    /// Interval between pushes; a push is also the heartbeat of the live status.
    pub interval: Duration,
}

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
    pub litellm_management_key: SecretString,
    /// Whether the management credential is the LiteLLM master key (fallback).
    pub litellm_management_uses_master: bool,
    /// Origin of the tunnel listener as agents and the identity provider see it, without slash.
    pub public_url: String,
    /// Origin LiteLLM uses to reach `api_listen`, without trailing slash.
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
    /// Credentials for the catalogue notification; `None` leaves auth-service to its own sync.
    pub catalogue: Option<CatalogueCredentials>,
    /// The inventory push and settings channel; without it models are served unpriced and
    /// unlimited and nothing is reported.
    pub inventory: Option<InventoryConfig>,
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

        let management_key = match env.secret("RELAY_LITELLM_API_KEY")? {
            Some(key) => (key, false),
            None => (env.required_secret("RELAY_LITELLM_MASTER_KEY")?, true),
        };

        Ok(Self {
            tunnel_listen: socket_addr(env, "RELAY_TUNNEL_LISTEN")?,
            api_listen: socket_addr(env, "RELAY_API_LISTEN")?,
            litellm_url,
            litellm_management_key: management_key.0,
            litellm_management_uses_master: management_key.1,
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
            catalogue: catalogue_credentials(env)?,
            inventory: inventory_config(env)?,
        })
    }
}

fn catalogue_credentials<F: Fn(&str) -> Option<String>>(
    env: &Env<F>,
) -> Result<Option<CatalogueCredentials>, ConfigError> {
    let client_id = env.optional("RELAY_CATALOGUE_CLIENT_ID")?;
    let client_secret = env.secret("RELAY_CATALOGUE_CLIENT_SECRET")?;
    match (client_id, client_secret) {
        (None, None) => Ok(None),
        (Some(client_id), Some(client_secret)) => {
            if client_id.trim().is_empty() {
                return Err(ConfigError::invalid(
                    "RELAY_CATALOGUE_CLIENT_ID",
                    "must not be empty",
                ));
            }
            Ok(Some(CatalogueCredentials {
                client_id: client_id.trim().to_owned(),
                client_secret,
            }))
        }
        _ => Err(ConfigError::invalid(
            "RELAY_CATALOGUE_CLIENT_ID",
            "RELAY_CATALOGUE_CLIENT_ID and RELAY_CATALOGUE_CLIENT_SECRET must be set together",
        )),
    }
}

fn inventory_config<F: Fn(&str) -> Option<String>>(
    env: &Env<F>,
) -> Result<Option<InventoryConfig>, ConfigError> {
    let Some(url) = env.optional("RELAY_INVENTORY_URL")? else {
        for key in ["RELAY_INVENTORY_SECRET", "RELAY_INVENTORY_RELAY_ID"] {
            if env.optional(key)?.is_some() || env.optional(&format!("{key}_FILE"))?.is_some() {
                return Err(ConfigError::invalid(key, "needs RELAY_INVENTORY_URL"));
            }
        }
        return Ok(None);
    };
    let relay_id = env.required("RELAY_INVENTORY_RELAY_ID")?;
    let valid_id = (1..=63).contains(&relay_id.len())
        && relay_id.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && relay_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !valid_id {
        return Err(ConfigError::invalid(
            "RELAY_INVENTORY_RELAY_ID",
            "expected 1 to 63 lowercase letters, digits or dashes",
        ));
    }
    Ok(Some(InventoryConfig {
        url: parse_https_url("RELAY_INVENTORY_URL", &url)?,
        secret: env.required_secret("RELAY_INVENTORY_SECRET")?,
        relay_id,
        interval: env.duration_secs("RELAY_INVENTORY_INTERVAL_SECS", 15)?,
    }))
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
        assert!(cfg.litellm_management_uses_master);
        assert_eq!(cfg.litellm_provider, "hosted_vllm");
        assert_eq!(cfg.max_tunnels_per_user, 64);
        assert_eq!(cfg.auth_failure_threshold, 5);
        assert_eq!(cfg.auth_failure_window, Duration::from_secs(60));
        assert_eq!(cfg.auth_lockout, Duration::from_secs(60));
    }

    #[test]
    fn catalogue_notification_is_off_without_credentials() {
        assert!(load(&BASE).unwrap().catalogue.is_none());
    }

    #[test]
    fn catalogue_notification_reads_the_client_secret_from_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "relay-secret\n").unwrap();
        let file = file.display().to_string();
        let pairs: Vec<_> = BASE
            .iter()
            .copied()
            .chain([
                ("RELAY_CATALOGUE_CLIENT_ID", "reverse-relay"),
                ("RELAY_CATALOGUE_CLIENT_SECRET_FILE", file.as_str()),
            ])
            .collect();
        let credentials = load(&pairs).unwrap().catalogue.unwrap();
        assert_eq!(credentials.client_id, "reverse-relay");
        assert_eq!(credentials.client_secret.expose(), "relay-secret");
    }

    #[test]
    fn catalogue_credentials_are_all_or_nothing() {
        let id_only: Vec<_> = BASE
            .iter()
            .copied()
            .chain([("RELAY_CATALOGUE_CLIENT_ID", "reverse-relay")])
            .collect();
        assert!(load(&id_only).is_err());
        let secret_only: Vec<_> = BASE
            .iter()
            .copied()
            .chain([("RELAY_CATALOGUE_CLIENT_SECRET", "relay-secret")])
            .collect();
        assert!(load(&secret_only).is_err());
        let blank_id: Vec<_> = BASE
            .iter()
            .copied()
            .chain([
                ("RELAY_CATALOGUE_CLIENT_ID", "  "),
                ("RELAY_CATALOGUE_CLIENT_SECRET", "relay-secret"),
            ])
            .collect();
        assert!(load(&blank_id).is_err());
    }

    #[test]
    fn a_dedicated_management_key_replaces_the_master_key() {
        let pairs: Vec<_> = BASE
            .iter()
            .filter(|(k, _)| *k != "RELAY_LITELLM_MASTER_KEY")
            .copied()
            .chain([("RELAY_LITELLM_API_KEY", "sk-management")])
            .collect();
        let cfg = load(&pairs).unwrap();
        assert!(!cfg.litellm_management_uses_master);
        assert_eq!(cfg.litellm_management_key.expose(), "sk-management");
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

    const INVENTORY: [(&str, &str); 3] = [
        (
            "RELAY_INVENTORY_URL",
            "https://raiversed.example.com/webhooks/relay/inventory",
        ),
        ("RELAY_INVENTORY_SECRET", "shared"),
        ("RELAY_INVENTORY_RELAY_ID", "relay-eu-1"),
    ];

    #[test]
    fn the_inventory_channel_is_off_by_default_and_complete_when_on() {
        assert!(load(&BASE).unwrap().inventory.is_none());
        let mut pairs = BASE.to_vec();
        pairs.extend(INVENTORY);
        let inventory = load(&pairs).unwrap().inventory.unwrap();
        assert_eq!(
            inventory.url,
            "https://raiversed.example.com/webhooks/relay/inventory"
        );
        assert_eq!(inventory.relay_id, "relay-eu-1");
        assert_eq!(inventory.interval, Duration::from_secs(15));
    }

    #[test]
    fn a_half_configured_inventory_channel_is_refused() {
        for kept in [[0, 1], [0, 2], [1, 2]] {
            let mut pairs = BASE.to_vec();
            pairs.extend(kept.map(|i| INVENTORY[i]));
            assert!(load(&pairs).is_err(), "{kept:?}");
        }
        let mut pairs = BASE.to_vec();
        pairs.push(INVENTORY[1]);
        assert!(load(&pairs).is_err());
    }

    #[test]
    fn the_inventory_url_must_be_https_and_the_relay_id_a_slug() {
        let mut pairs = BASE.to_vec();
        pairs.extend(INVENTORY);
        pairs[BASE.len()] = ("RELAY_INVENTORY_URL", "http://raiversed.example.com/x");
        assert!(load(&pairs).is_err());
        pairs[BASE.len()] = INVENTORY[0];
        pairs[BASE.len() + 2] = ("RELAY_INVENTORY_RELAY_ID", "Relay EU");
        assert!(load(&pairs).is_err());
    }
}
