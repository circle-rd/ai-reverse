//! Verifies the access tokens agents present when opening a tunnel.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use http::{HeaderMap, header};
use reverse_core::ids::{AgentId, ClientId};
use reverse_core::wire::AGENT_ID_HEADER;
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;

const KEYS_TTL: Duration = Duration::from_secs(300);
const KID_REFETCH_COOLDOWN: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_JWKS_BYTES: usize = 256 * 1024;
const LEEWAY_SECS: u64 = 60;

/// Why a tunnel upgrade carried no usable identity.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// The authorization header is absent or not a Bearer credential.
    #[error("missing or malformed bearer credential")]
    MissingToken,
    /// The agent id header is absent or invalid.
    #[error("invalid agent id: {0}")]
    AgentId(#[from] reverse_core::ids::IdError),
    /// The token header cannot be decoded.
    #[error("invalid token header: {0}")]
    Header(#[from] jsonwebtoken::errors::Error),
    /// The signing key named by the token is not published.
    #[error("unknown signing key")]
    UnknownKey,
    /// The token does not verify.
    #[error("invalid token")]
    InvalidToken,
    /// The token lacks the required scope.
    #[error("missing scope")]
    MissingScope,
    /// The identity provider could not be reached or answered nonsense.
    #[error("identity provider failure: {0}")]
    Provider(String),
    /// The token carries no `preferred_username`, so no account can be derived.
    #[error("token has no preferred_username")]
    MissingUsername,
    /// The account name cannot be used as a model prefix.
    #[error("invalid account name: {0}")]
    Account(String),
}

/// The verified identity behind one tunnel.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Immutable subject id.
    pub sub: String,
    /// Model-alias prefix: `u-` plus the lowercase `preferred_username`.
    pub account: ClientId,
    /// Stable per-agent instance id.
    pub agent_id: AgentId,
}

impl Identity {
    /// Internal node id, unique per account and agent instance.
    #[must_use]
    pub fn node_id(&self) -> String {
        format!("{}:{}", self.sub, self.agent_id)
    }
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    #[serde(default)]
    preferred_username: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

#[derive(Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

#[derive(Deserialize)]
struct Jwk {
    #[serde(default)]
    kid: Option<String>,
    kty: String,
    #[serde(default)]
    alg: Option<String>,
    #[serde(default)]
    crv: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

struct SigningKey {
    kid: String,
    algorithm: jsonwebtoken::Algorithm,
    key: jsonwebtoken::DecodingKey,
}

struct CachedKeys {
    keys: Vec<SigningKey>,
    kids: HashSet<String>,
    fetched: Instant,
}

/// Verifies tunnel tokens against the identity provider's published keys.
pub struct AuthState {
    http: reqwest::Client,
    issuer: String,
    audience: String,
    scope: String,
    cache: Mutex<Cache>,
    refresh: tokio::sync::Mutex<()>,
}

#[derive(Default)]
struct Cache {
    keys: Option<CachedKeys>,
    jwks_uri: Option<String>,
    last_attempt: Option<Instant>,
}

impl AuthState {
    /// Builds the verifier; network access happens on the first upgrade.
    pub fn new(config: &Config) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            http,
            issuer: config.auth_issuer.as_str().trim_end_matches('/').to_owned(),
            audience: config.auth_audience.clone(),
            scope: config.auth_scope.clone(),
            cache: Mutex::new(Cache::default()),
            refresh: tokio::sync::Mutex::new(()),
        })
    }

    /// Validates the upgrade headers and returns the agent identity.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<Identity, AuthError> {
        let token = bearer(headers).ok_or(AuthError::MissingToken)?;
        let agent_id = headers
            .get(AGENT_ID_HEADER)
            .and_then(|value| value.to_str().ok())
            .ok_or(AuthError::AgentId(
                reverse_core::ids::IdError::AgentIdLength,
            ))?;
        let agent_id = AgentId::parse(agent_id)?;

        let token_header = jsonwebtoken::decode_header(token)?;
        let kid = token_header.kid.ok_or(AuthError::UnknownKey)?;
        let algorithm = allowed_algorithm(token_header.alg)?;
        let key = self.signing_key(&kid, algorithm).await?;

        let mut validation = jsonwebtoken::Validation::new(algorithm);
        validation.set_audience(&[self.audience.as_str()]);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.leeway = LEEWAY_SECS;
        validation.validate_exp = true;
        let claims = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .map_err(|_| AuthError::InvalidToken)?
            .claims;

        let granted = claims.scope.as_deref().unwrap_or_default();
        if !granted.split(' ').any(|scope| scope == self.scope) {
            return Err(AuthError::MissingScope);
        }
        let account = account_of(claims.preferred_username.as_deref())?;
        Ok(Identity {
            sub: claims.sub,
            account,
            agent_id,
        })
    }

    async fn signing_key(
        &self,
        kid: &str,
        algorithm: jsonwebtoken::Algorithm,
    ) -> Result<jsonwebtoken::DecodingKey, AuthError> {
        if let Some(keys) = self.cached(kid) {
            return select(&keys, kid, algorithm);
        }
        let _guard = self.refresh.lock().await;
        if let Some(keys) = self.cached_after_lock() {
            return select(&keys, kid, algorithm);
        }
        let keys = self.fetch().await?;
        let keys = Arc::new(keys);
        {
            let mut cache = lock(&self.cache);
            cache.last_attempt = Some(Instant::now());
            cache.keys = Some(CachedKeys {
                kids: keys.iter().map(|key| key.kid.clone()).collect(),
                keys: keys
                    .iter()
                    .map(|key| SigningKey {
                        kid: key.kid.clone(),
                        algorithm: key.algorithm,
                        key: key.key.clone(),
                    })
                    .collect(),
                fetched: Instant::now(),
            });
        }
        select(&keys, kid, algorithm)
    }

    /// Returns a fresh enough cache when it can answer this `kid`.
    fn cached(&self, kid: &str) -> Option<Vec<SigningKey>> {
        let cache = lock(&self.cache);
        let keys = cache.keys.as_ref()?;
        let fresh = keys.fetched.elapsed() < KEYS_TTL;
        let known = keys.kids.contains(kid);
        let cooling = cache
            .last_attempt
            .is_some_and(|at| at.elapsed() < KID_REFETCH_COOLDOWN);
        if fresh && (known || cooling) {
            return Some(clone_keys(keys));
        }
        None
    }

    /// Re-checks the cache once the refresh lock is held.
    fn cached_after_lock(&self) -> Option<Vec<SigningKey>> {
        let cache = lock(&self.cache);
        let keys = cache.keys.as_ref()?;
        (keys.fetched.elapsed() < KEYS_TTL).then(|| clone_keys(keys))
    }

    async fn fetch(&self) -> Result<Vec<SigningKey>, AuthError> {
        let jwks_uri = {
            let cached = lock(&self.cache).jwks_uri.clone();
            match cached {
                Some(uri) => uri,
                None => {
                    let url = format!("{}/.well-known/openid-configuration", self.issuer);
                    let discovery: Discovery = self
                        .http
                        .get(url)
                        .send()
                        .await
                        .map_err(|error| AuthError::Provider(error.to_string()))?
                        .error_for_status()
                        .map_err(|error| AuthError::Provider(error.to_string()))?
                        .json()
                        .await
                        .map_err(|error| AuthError::Provider(error.to_string()))?;
                    if discovery.issuer.trim_end_matches('/') != self.issuer {
                        return Err(AuthError::Provider("issuer mismatch".into()));
                    }
                    lock(&self.cache).jwks_uri = Some(discovery.jwks_uri.clone());
                    discovery.jwks_uri
                }
            }
        };
        let body = self
            .http
            .get(jwks_uri)
            .send()
            .await
            .map_err(|error| AuthError::Provider(error.to_string()))?
            .error_for_status()
            .map_err(|error| AuthError::Provider(error.to_string()))?
            .bytes()
            .await
            .map_err(|error| AuthError::Provider(error.to_string()))?;
        if body.len() > MAX_JWKS_BYTES {
            return Err(AuthError::Provider("jwks too large".into()));
        }
        let set: JwkSet = serde_json::from_slice(&body)
            .map_err(|error| AuthError::Provider(format!("invalid jwks: {error}")))?;
        let keys: Vec<SigningKey> = set.keys.iter().filter_map(parse_jwk).collect();
        if keys.is_empty() {
            return Err(AuthError::Provider("no usable signing key".into()));
        }
        tracing::debug!(count = keys.len(), "auth.keys_refreshed");
        Ok(keys)
    }
}

fn account_of(preferred_username: Option<&str>) -> Result<ClientId, AuthError> {
    let username = preferred_username.ok_or(AuthError::MissingUsername)?;
    ClientId::account(username).map_err(|error| AuthError::Account(error.to_string()))
}

fn lock(cache: &Mutex<Cache>) -> MutexGuard<'_, Cache> {
    // Poisoning can only follow a panic; release builds abort on panic.
    cache.lock().expect("auth cache poisoned")
}

fn clone_keys(keys: &CachedKeys) -> Vec<SigningKey> {
    keys.keys
        .iter()
        .map(|key| SigningKey {
            kid: key.kid.clone(),
            algorithm: key.algorithm,
            key: key.key.clone(),
        })
        .collect()
}

fn select(
    keys: &[SigningKey],
    kid: &str,
    algorithm: jsonwebtoken::Algorithm,
) -> Result<jsonwebtoken::DecodingKey, AuthError> {
    keys.iter()
        .find(|key| key.kid == kid && key.algorithm == algorithm)
        .map(|key| key.key.clone())
        .ok_or(AuthError::UnknownKey)
}

fn allowed_algorithm(
    algorithm: jsonwebtoken::Algorithm,
) -> Result<jsonwebtoken::Algorithm, AuthError> {
    match algorithm {
        jsonwebtoken::Algorithm::EdDSA
        | jsonwebtoken::Algorithm::ES256
        | jsonwebtoken::Algorithm::RS256 => Ok(algorithm),
        _ => Err(AuthError::Header(jsonwebtoken::errors::Error::from(
            jsonwebtoken::errors::ErrorKind::InvalidAlgorithm,
        ))),
    }
}

fn parse_jwk(jwk: &Jwk) -> Option<SigningKey> {
    let kid = jwk.kid.clone()?;
    let algorithm = match jwk.alg.as_deref() {
        Some("EdDSA") => jsonwebtoken::Algorithm::EdDSA,
        Some("ES256") => jsonwebtoken::Algorithm::ES256,
        Some("RS256") => jsonwebtoken::Algorithm::RS256,
        Some(_) => return None,
        None => match jwk.kty.as_str() {
            "OKP" if jwk.crv.as_deref() == Some("Ed25519") => jsonwebtoken::Algorithm::EdDSA,
            "EC" if jwk.crv.as_deref() == Some("P-256") => jsonwebtoken::Algorithm::ES256,
            "RSA" => jsonwebtoken::Algorithm::RS256,
            _ => return None,
        },
    };
    let key = match algorithm {
        jsonwebtoken::Algorithm::EdDSA => {
            let x = b64(jwk.x.as_deref()?)?;
            if x.len() != 32 {
                return None;
            }
            let mut der = vec![
                0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
            ];
            der.extend_from_slice(&x);
            jsonwebtoken::DecodingKey::from_ed_der(&der)
        }
        jsonwebtoken::Algorithm::ES256 => {
            jsonwebtoken::DecodingKey::from_ec_components(jwk.x.as_deref()?, jwk.y.as_deref()?)
                .ok()?
        }
        jsonwebtoken::Algorithm::RS256 => {
            jsonwebtoken::DecodingKey::from_rsa_components(jwk.n.as_deref()?, jwk.e.as_deref()?)
                .ok()?
        }
        _ => return None,
    };
    Some(SigningKey {
        kid,
        algorithm,
        key,
    })
}

fn b64(value: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(rest.trim())
        .filter(|token| !token.is_empty())
}

#[allow(dead_code)]
fn _claims_shape(value: &Value) -> &Value {
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_symmetric_free_algorithms_are_accepted() {
        for algorithm in [
            jsonwebtoken::Algorithm::EdDSA,
            jsonwebtoken::Algorithm::ES256,
            jsonwebtoken::Algorithm::RS256,
        ] {
            assert!(allowed_algorithm(algorithm).is_ok());
        }
        assert!(allowed_algorithm(jsonwebtoken::Algorithm::HS256).is_err());
    }

    #[test]
    fn the_account_is_the_lowercase_username_under_the_u_prefix() {
        assert_eq!(account_of(Some("Bmz")).unwrap().as_str(), "u-bmz");
    }

    #[test]
    fn a_token_without_a_username_is_refused() {
        assert!(matches!(account_of(None), Err(AuthError::MissingUsername)));
        assert!(matches!(account_of(Some("")), Err(AuthError::Account(_))));
    }

    #[test]
    fn a_username_that_cannot_prefix_a_model_is_refused() {
        for name in ["a/b", "a b", &"x".repeat(64)] {
            assert!(
                matches!(account_of(Some(name)), Err(AuthError::Account(_))),
                "{name}"
            );
        }
    }

    #[test]
    fn errors_never_echo_the_credential() {
        let error = jsonwebtoken::decode_header("sentinel-token").unwrap_err();
        let wrapped = AuthError::Header(error);
        assert!(!wrapped.to_string().contains("sentinel-token"));
    }

    #[test]
    fn the_agent_id_header_must_be_present_and_valid() {
        let headers = HeaderMap::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        let state = AuthState {
            http: reqwest::Client::new(),
            issuer: "http://127.0.0.1:1".into(),
            audience: "http://relay.test".into(),
            scope: "tunnel:serve".into(),
            cache: Mutex::new(Cache::default()),
            refresh: tokio::sync::Mutex::new(()),
        };
        let error = runtime.block_on(state.authenticate(&headers)).unwrap_err();
        assert!(matches!(error, AuthError::MissingToken));
    }
}
