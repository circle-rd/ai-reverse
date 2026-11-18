//! Constant-time comparison for the API key.

use subtle::ConstantTimeEq;

/// Constant-time equality for secrets of any length.
#[must_use]
pub fn secrets_match(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparison_behaves() {
        assert!(secrets_match("same", "same"));
        assert!(!secrets_match("same", "sam"));
        assert!(!secrets_match("same", "same "));
    }
}
