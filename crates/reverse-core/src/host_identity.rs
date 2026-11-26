//! Validated, self-reported host identity for owner-only inventory.

use ring::hmac;
use serde::{Deserialize, Serialize};

const MACHINE_DOMAIN: &[u8] = b"ai-reverse/machine-id/v1";
const OWNER_DOMAIN: &str = "ai-reverse/owner-host/v1:";

/// A pseudonymous host identifier, scoped to an owner before publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostId(String);

impl HostId {
    /// Derives an application-specific pseudonym without publishing the machine id.
    pub fn from_machine_id(raw: &str) -> Result<Self, &'static str> {
        let raw = raw.trim();
        if raw.len() != 32
            || !raw
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || raw.bytes().all(|b| b == b'0')
        {
            return Err("expected a nonzero, lowercase 32-character Linux machine-id");
        }
        Ok(Self(digest(raw.as_bytes(), MACHINE_DOMAIN)))
    }

    /// Produces a different identifier for each verified owner.
    #[must_use]
    pub fn for_owner(&self, owner: &str) -> Self {
        Self(digest(
            self.0.as_bytes(),
            format!("{OWNER_DOMAIN}{owner}").as_bytes(),
        ))
    }
}

fn digest(key: &[u8], message: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, key);
    hmac::sign(&key, message)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl TryFrom<String> for HostId {
    type Error = &'static str;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        if raw.len() != 64
            || !raw
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("expected a lowercase 64-character host pseudonym");
        }
        Ok(Self(raw))
    }
}

impl From<HostId> for String {
    fn from(value: HostId) -> Self {
        value.0
    }
}

/// An operator-declared local hostname, not a DNS name or IP address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostName(String);

impl TryFrom<String> for HostName {
    type Error = &'static str;

    fn try_from(raw: String) -> Result<Self, Self::Error> {
        let bytes = raw.as_bytes();
        let valid = (1..=63).contains(&bytes.len())
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            && bytes.iter().any(u8::is_ascii_alphabetic);
        let numeric_groups = raw
            .split('-')
            .filter(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
            .count();
        if !valid || numeric_groups >= 4 {
            return Err("expected a local hostname, not an IP address or DNS name");
        }
        Ok(Self(raw))
    }
}

impl From<HostName> for String {
    fn from(value: HostName) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pseudonyms_are_stable_and_owner_scoped() {
        let host = HostId::from_machine_id("0123456789abcdef0123456789abcdef").unwrap();
        let other = HostId::from_machine_id("1123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(host.for_owner("owner"), host.for_owner("owner"));
        assert_ne!(host.for_owner("owner"), host.for_owner("other"));
        assert_ne!(host.for_owner("owner"), other.for_owner("owner"));
        assert!(HostId::from_machine_id("00000000000000000000000000000000").is_err());
        assert!(serde_json::from_str::<HostId>("\"invalid\"").is_err());
    }

    #[test]
    fn hostnames_reject_addresses_and_dns_names() {
        assert!(HostName::try_from("node-1".to_owned()).is_ok());
        for invalid in ["192.0.2.1", "node.example.com", "ip-192-0-2-1", "bad\nname"] {
            assert!(HostName::try_from(invalid.to_owned()).is_err(), "{invalid}");
        }
    }
}
