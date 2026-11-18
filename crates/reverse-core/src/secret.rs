//! Secret material that never reaches logs or `Debug` output.

use std::fmt;

use http::HeaderValue;
use thiserror::Error;
use zeroize::Zeroizing;

/// A secret held in a zeroizing buffer.
#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

/// The secret cannot be sent as an HTTP header.
#[derive(Debug, Error)]
#[error("secret contains characters that are not valid in an HTTP header")]
pub struct InvalidHeaderSecret;

impl SecretString {
    /// Wraps a secret value.
    #[must_use]
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// The raw secret; callers must not log it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// An `Authorization: Bearer` header value marked sensitive.
    pub fn bearer_header(&self) -> Result<HeaderValue, InvalidHeaderSecret> {
        let raw = Zeroizing::new(format!("Bearer {}", self.0.as_str()));
        let mut value = HeaderValue::from_str(&raw).map_err(|_| InvalidHeaderSecret)?;
        value.set_sensitive(true);
        Ok(value)
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString(<redacted>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_prints_the_secret() {
        let secret = SecretString::new("hunter2".to_owned());
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[test]
    fn bearer_header_is_sensitive() {
        let header = SecretString::new("abc".to_owned()).bearer_header().unwrap();
        assert!(header.is_sensitive());
        assert_eq!(header.as_bytes(), b"Bearer abc");
    }

    #[test]
    fn rejects_header_injection() {
        assert!(
            SecretString::new("a\r\nb".to_owned())
                .bearer_header()
                .is_err()
        );
    }
}
