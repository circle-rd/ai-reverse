//! Typed, validated configuration read from the environment once at startup.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use http::Uri;
use http::uri::{Authority, Scheme};
use thiserror::Error;
use zeroize::Zeroizing;

use crate::secret::SecretString;

/// A fatal configuration problem.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// A required setting is absent.
    #[error("{0} is required")]
    Missing(String),
    /// A setting is present but empty.
    #[error("{0} must not be empty")]
    Empty(String),
    /// A secret was given both inline and through a file.
    #[error("{0} and {0}_FILE cannot both be set")]
    BothSecretForms(String),
    /// A secret file could not be read.
    #[error("cannot read {key}: {source}")]
    SecretFile {
        /// The `*_FILE` variable.
        key: String,
        /// The underlying I/O error.
        #[source]
        source: io::Error,
    },
    /// A setting has an unusable value.
    #[error("{key} is invalid: {reason}")]
    Invalid {
        /// The offending variable.
        key: String,
        /// Why the value was rejected.
        reason: String,
    },
}

impl ConfigError {
    /// Builds an [`ConfigError::Invalid`].
    pub fn invalid(key: &str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            key: key.to_owned(),
            reason: reason.into(),
        }
    }
}

/// Reads settings through an injectable lookup so tests never touch the process environment.
pub struct Env<F> {
    lookup: F,
}

/// The real process environment.
#[must_use]
pub fn process_env() -> Env<impl Fn(&str) -> Option<String>> {
    Env::new(|key| std::env::var(key).ok())
}

impl<F: Fn(&str) -> Option<String>> Env<F> {
    /// Wraps a lookup function.
    pub fn new(lookup: F) -> Self {
        Self { lookup }
    }

    /// An optional setting; present-but-empty is an error.
    pub fn optional(&self, key: &str) -> Result<Option<String>, ConfigError> {
        match (self.lookup)(key) {
            None => Ok(None),
            Some(value) if value.trim().is_empty() => Err(ConfigError::Empty(key.to_owned())),
            Some(value) => Ok(Some(value)),
        }
    }

    /// A required setting.
    pub fn required(&self, key: &str) -> Result<String, ConfigError> {
        self.optional(key)?
            .ok_or_else(|| ConfigError::Missing(key.to_owned()))
    }

    /// A secret from `KEY` or `KEY_FILE`, never both.
    pub fn secret(&self, key: &str) -> Result<Option<SecretString>, ConfigError> {
        let file_key = format!("{key}_FILE");
        match (self.optional(key)?, self.optional(&file_key)?) {
            (Some(_), Some(_)) => Err(ConfigError::BothSecretForms(key.to_owned())),
            (Some(value), None) => Ok(Some(SecretString::new(value))),
            (None, Some(path)) => {
                let raw = std::fs::read_to_string(&path)
                    .map(Zeroizing::new)
                    .map_err(|source| ConfigError::SecretFile {
                        key: file_key.clone(),
                        source,
                    })?;
                let trimmed = raw.trim_end_matches(['\n', '\r']);
                if trimmed.is_empty() {
                    return Err(ConfigError::Empty(file_key));
                }
                Ok(Some(SecretString::new(trimmed.to_owned())))
            }
            (None, None) => Ok(None),
        }
    }

    /// A secret that must be present.
    pub fn required_secret(&self, key: &str) -> Result<SecretString, ConfigError> {
        self.secret(key)?
            .ok_or_else(|| ConfigError::Missing(format!("{key} or {key}_FILE")))
    }

    /// A strictly positive number of seconds.
    pub fn duration_secs(&self, key: &str, default: u64) -> Result<Duration, ConfigError> {
        let secs = match self.optional(key)? {
            None => default,
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| ConfigError::invalid(key, "expected a whole number of seconds"))?,
        };
        if secs == 0 {
            return Err(ConfigError::invalid(key, "must be greater than zero"));
        }
        Ok(Duration::from_secs(secs))
    }

    /// A strictly positive byte count.
    pub fn bytes(&self, key: &str, default: usize) -> Result<usize, ConfigError> {
        let value = match self.optional(key)? {
            None => default,
            Some(raw) => raw
                .trim()
                .parse::<usize>()
                .map_err(|_| ConfigError::invalid(key, "expected a whole number of bytes"))?,
        };
        if value == 0 {
            return Err(ConfigError::invalid(key, "must be greater than zero"));
        }
        Ok(value)
    }

    /// A boolean that is `false` when unset and otherwise strictly `true` or `false`.
    pub fn flag(&self, key: &str) -> Result<bool, ConfigError> {
        match self.optional(key)?.as_deref() {
            None | Some("false") => Ok(false),
            Some("true") => Ok(true),
            Some(_) => Err(ConfigError::invalid(key, "expected true or false")),
        }
    }
}

/// An `http(s)://host[:port]` origin without credentials, path or query.
#[derive(Debug, Clone)]
pub struct BaseUrl {
    scheme: Scheme,
    authority: Authority,
}

impl BaseUrl {
    /// Parses and validates an origin.
    pub fn parse(key: &str, raw: &str) -> Result<Self, ConfigError> {
        let uri: Uri = raw
            .trim()
            .parse()
            .map_err(|_| ConfigError::invalid(key, "not a valid URL"))?;
        let scheme = uri
            .scheme()
            .filter(|s| **s == Scheme::HTTP || **s == Scheme::HTTPS)
            .cloned()
            .ok_or_else(|| ConfigError::invalid(key, "scheme must be http or https"))?;
        let authority = uri
            .authority()
            .cloned()
            .ok_or_else(|| ConfigError::invalid(key, "host is missing"))?;
        if authority.as_str().contains('@') {
            return Err(ConfigError::invalid(
                key,
                "credentials are not allowed in the URL",
            ));
        }
        if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
            return Err(ConfigError::invalid(key, "path and query are not allowed"));
        }
        Ok(Self { scheme, authority })
    }

    /// `http` or `https`.
    #[must_use]
    pub fn scheme(&self) -> &Scheme {
        &self.scheme
    }

    /// `host[:port]`.
    #[must_use]
    pub fn authority(&self) -> &Authority {
        &self.authority
    }

    /// The host without brackets.
    #[must_use]
    pub fn host(&self) -> &str {
        self.authority
            .host()
            .trim_start_matches('[')
            .trim_end_matches(']')
    }

    /// The explicit port or the scheme default.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.authority
            .port_u16()
            .unwrap_or(if self.scheme == Scheme::HTTPS {
                443
            } else {
                80
            })
    }

    /// Whether the host is `localhost` or a loopback address.
    #[must_use]
    pub fn is_loopback(&self) -> bool {
        self.host().eq_ignore_ascii_case("localhost")
            || self
                .host()
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Env<impl Fn(&str) -> Option<String> + use<>> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        Env::new(move |key| map.get(key).cloned())
    }

    #[test]
    fn required_reports_missing_and_empty() {
        assert!(matches!(
            env(&[]).required("X"),
            Err(ConfigError::Missing(_))
        ));
        assert!(matches!(
            env(&[("X", "  ")]).required("X"),
            Err(ConfigError::Empty(_))
        ));
        assert_eq!(env(&[("X", "v")]).required("X").unwrap(), "v");
    }

    #[test]
    fn secret_rejects_both_forms() {
        let e = env(&[("S", "a"), ("S_FILE", "/nope")]);
        assert!(matches!(
            e.secret("S"),
            Err(ConfigError::BothSecretForms(_))
        ));
    }

    #[test]
    fn secret_reads_file_and_trims_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "s3cret\n").unwrap();
        let e = env(&[("S_FILE", path.to_str().unwrap())]);
        assert_eq!(e.secret("S").unwrap().unwrap().expose(), "s3cret");
    }

    #[test]
    fn secret_rejects_empty_file_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("key");
        std::fs::write(&path, "\n").unwrap();
        let empty = env(&[("S_FILE", path.to_str().unwrap())]);
        assert!(matches!(empty.secret("S"), Err(ConfigError::Empty(_))));
        let missing = env(&[("S_FILE", "/definitely/not/here")]);
        assert!(matches!(
            missing.secret("S"),
            Err(ConfigError::SecretFile { .. })
        ));
    }

    #[test]
    fn durations_and_flags_are_strict() {
        let e = env(&[("D", "0"), ("E", "abc"), ("F", "yes"), ("G", "true")]);
        assert!(e.duration_secs("D", 5).is_err());
        assert!(e.duration_secs("E", 5).is_err());
        assert_eq!(e.duration_secs("UNSET", 7).unwrap(), Duration::from_secs(7));
        assert!(e.flag("F").is_err());
        assert!(e.flag("G").unwrap());
        assert!(!e.flag("UNSET").unwrap());
    }

    #[test]
    fn base_url_accepts_plain_origins() {
        let url = BaseUrl::parse("K", "http://127.0.0.1:8443").unwrap();
        assert_eq!(url.host(), "127.0.0.1");
        assert_eq!(url.port(), 8443);
        assert!(url.is_loopback());
        let remote = BaseUrl::parse("K", "https://relay.example.com").unwrap();
        assert_eq!(remote.port(), 443);
        assert!(!remote.is_loopback());
        assert!(
            BaseUrl::parse("K", "http://[::1]:1/")
                .unwrap()
                .is_loopback()
        );
    }

    #[test]
    fn base_url_rejects_everything_else() {
        for raw in [
            "ftp://x",
            "relay:80",
            "http://u:p@host",
            "http://host/path",
            "http://host/?q=1",
            "",
        ] {
            assert!(BaseUrl::parse("K", raw).is_err(), "{raw}");
        }
    }
}
