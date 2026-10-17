//! TLS for `chronos serve`, over HTTP and the Postgres protocol: a certificate chain and its
//! private key, in PEM files (`--tls-cert` and `--tls-key`, or CHRONOS_TLS_CERT and CHRONOS_TLS_KEY).

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use std::io;
use std::sync::Arc;

/// A server's TLS: its settings, and its certificate's tls-server-end-point hash, which a
/// Postgres client logging in with SCRAM-SHA-256-PLUS binds to (None: the certificate is signed
/// with a hash channel binding can't use; see `chronos::sql::scram::end_point`).
#[derive(Clone)]
pub struct Tls {
    pub cfg: Arc<ServerConfig>,
    pub end_point: Option<Vec<u8>>,
}

pub fn config(cert: &str, key: &str) -> io::Result<Tls> {
    let bad = |what: &str, file: &str, e: &dyn std::fmt::Display| io::Error::other(format!("{what} {file}: {e}"));
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|i| i.collect::<Result<Vec<_>, _>>())
        .map_err(|e| bad("reading the certificate", cert, &e))?;
    let Some(end_point) = certs.first().map(|c| chronos::sql::scram::end_point(c)) else {
        return Err(io::Error::other(format!("{cert} has no certificate (expected PEM: -----BEGIN CERTIFICATE-----)")));
    };
    let key = PrivateKeyDer::from_pem_file(key).map_err(|e| bad("reading the private key", key, &e))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|b| b.with_no_client_auth().with_single_cert(certs, key))
        .map_err(|e| io::Error::other(format!("TLS: {e}")))?;
    Ok(Tls { cfg: Arc::new(cfg), end_point })
}
