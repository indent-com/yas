//! The TLS client configuration YAS builds itself (TURN over TLS, `wss://`
//! signaling and edges), on an explicit rustls provider rather than whatever
//! rustls would pick: a program with both of rustls's providers compiled in
//! and none installed would otherwise panic on its first connection.

use rustls::crypto::CryptoProvider;
use std::sync::{Arc, OnceLock};

#[cfg(not(any(feature = "ring", feature = "aws-lc-rs")))]
compile_error!(
    "yas-webrtc-forwarder needs a rustls provider: enable its `ring` or `aws-lc-rs` feature"
);

/// The provider YAS's TLS clients use: the program's process-wide default when
/// it installed one, else the one this build enables (aws-lc-rs when it has
/// both).
pub fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(build_provider()))
}

/// The provider this build enables, whatever the process default is.
#[cfg(feature = "aws-lc-rs")]
pub fn build_provider() -> CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

/// The provider this build enables, whatever the process default is.
#[cfg(all(feature = "ring", not(feature = "aws-lc-rs")))]
pub fn build_provider() -> CryptoProvider {
    rustls::crypto::ring::default_provider()
}

/// Install [`build_provider`] as the process-wide default unless the program
/// installed one already. For YAS's own daemons, never from a library path.
pub fn install_default_provider() {
    if CryptoProvider::get_default().is_none() {
        // Another thread may install one first: either way one is installed.
        let _ = build_provider().install_default();
    }
}

/// The platform's root certificates, read once per process (reading them is
/// not free).
pub fn native_roots() -> Arc<rustls::RootCertStore> {
    static ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    ROOTS
        .get_or_init(|| {
            let mut roots = rustls::RootCertStore::empty();
            for cert in rustls_native_certs::load_native_certs().certs {
                roots.add(cert).ok();
            }
            Arc::new(roots)
        })
        .clone()
}

/// A client configuration trusting [`native_roots`], on [`provider`], built
/// once per process.
pub fn client_config() -> Arc<rustls::ClientConfig> {
    static CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            Arc::new(
                rustls::ClientConfig::builder_with_provider(provider())
                    .with_safe_default_protocol_versions()
                    .expect("the provider supports TLS 1.2 and 1.3")
                    .with_root_certificates(native_roots())
                    .with_no_client_auth(),
            )
        })
        .clone()
}

/// The connector for `tokio_tungstenite::connect_async_tls_with_config`: TLS
/// on [`client_config`] for `wss://`, nothing for `ws://`.
pub fn websocket_connector() -> tokio_tungstenite::Connector {
    tokio_tungstenite::Connector::Rustls(client_config())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_config_uses_a_provider_with_tls13() {
        let config = client_config();
        assert!(
            config
                .crypto_provider()
                .cipher_suites
                .iter()
                .any(|suite| suite.tls13().is_some())
        );
    }
}
