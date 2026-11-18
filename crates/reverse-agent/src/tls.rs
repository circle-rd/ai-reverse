//! TLS client settings for dialing an `https` relay.

use std::sync::Arc;

use rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// A connector trusting the public web PKI, restricted to HTTP/1.1 for the upgrade.
pub fn connector() -> Result<TlsConnector, rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(config)))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_connector_builds() {
        assert!(super::connector().is_ok());
    }
}
