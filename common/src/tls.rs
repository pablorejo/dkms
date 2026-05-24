//! TLS / mTLS helpers shared by HTTP and gRPC stacks.
//!
//! Loads PEM cert chains + private keys from disk and wraps them in
//! `rustls::ServerConfig` / `rustls::ClientConfig`. Both DKMS HTTP (axum)
//! and tonic gRPC accept these.

use std::{fs::File, io::BufReader, path::Path, sync::Arc};

use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ClientConfig, RootCertStore, ServerConfig,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TlsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("rustls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("no private key in {0}")]
    NoKey(String),
    #[error("invalid pem in {0}")]
    BadPem(String),
}

pub fn load_certs(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let mut rd = BufReader::new(File::open(path)?);
    let certs: Result<Vec<_>, _> = rustls_pemfile::certs(&mut rd).collect();
    certs.map_err(|_| TlsError::BadPem(path.display().to_string()))
}

pub fn load_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let mut rd = BufReader::new(File::open(path)?);
    rustls_pemfile::private_key(&mut rd)?.ok_or_else(|| TlsError::NoKey(path.display().to_string()))
}

pub fn server_config(
    cert_path: &Path,
    key_path: &Path,
    client_ca: Option<&Path>,
) -> Result<Arc<ServerConfig>, TlsError> {
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;

    let builder = ServerConfig::builder();
    let mut cfg = if let Some(ca) = client_ca {
        let mut roots = RootCertStore::empty();
        for c in load_certs(ca)? {
            roots.add(c)?;
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| TlsError::BadPem(e.to_string()))?;
        builder
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)?
    } else {
        builder.with_no_client_auth().with_single_cert(certs, key)?
    };

    // ALPN: anunciar `h2` y `http/1.1`. El peer_client DKMS↔DKMS usa
    // reqwest con `http2_prior_knowledge() + use_rustls_tls()` que añade
    // alpn `h2` al ClientHello; si el servidor no lo lista, el handshake
    // termina en `NoApplicationProtocol` (visible como `tls handshake eof`
    // en logs). El plano SAE también puede llegar con HTTP/1.1 desde
    // clientes Python `requests` por intra-cluster, así que ambos van.
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(cfg))
}

pub fn client_config(ca_path: Option<&Path>) -> Result<Arc<ClientConfig>, TlsError> {
    let mut roots = RootCertStore::empty();
    if let Some(ca) = ca_path {
        for c in load_certs(ca)? {
            roots.add(c)?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}
