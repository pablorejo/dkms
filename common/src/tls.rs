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

    // Provider con ML-DSA (§Fase 5/6 PQC) además de RSA/ECDSA/EdDSA: acepta
    // certs post-cuánticos y clásicos (retrocompatible durante la migración).
    let provider = crate::tls_pqc::pqc_crypto_provider();
    let builder = ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(TlsError::Rustls)?;
    let mut cfg = if let Some(ca) = client_ca {
        let mut roots = RootCertStore::empty();
        for c in load_certs(ca)? {
            roots.add(c)?;
        }
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
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
    let roots = root_store(ca_path)?;
    let provider = crate::tls_pqc::pqc_crypto_provider();
    Ok(Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Rustls)?
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

/// Igual que [`client_config`] pero presentando un **cert de cliente** —
/// el lado cliente de un mTLS. Lo usan los módulos que se conectan al SDN
/// (o entre planos de control) firmados por la CA de red: el servidor
/// verifica quién llama por el SAN del cert.
///
/// `ca_path` = trust root del servidor (la misma CA de red); `cert_path`
/// / `key_path` = la identidad de este cliente. Si `ca_path` es `None`,
/// usa las raíces webpki del sistema (para servidores públicos).
pub fn client_config_mtls(
    ca_path: Option<&Path>,
    cert_path: &Path,
    key_path: &Path,
) -> Result<Arc<ClientConfig>, TlsError> {
    let roots = root_store(ca_path)?;
    let certs = load_certs(cert_path)?;
    let key = load_key(key_path)?;
    let provider = crate::tls_pqc::pqc_crypto_provider();
    Ok(Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(TlsError::Rustls)?
            .with_root_certificates(roots)
            .with_client_auth_cert(certs, key)?,
    ))
}

/// Construye un `RootCertStore` desde un PEM (que puede ser un *bundle*
/// multi-CA) o, si `None`, desde las raíces webpki del sistema.
fn root_store(ca_path: Option<&Path>) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    if let Some(ca) = ca_path {
        for c in load_certs(ca)? {
            roots.add(c)?;
        }
    } else {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn openssl(args: &[&str]) -> bool {
        Command::new("openssl")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// La API pública (`server_config`/`client_config_mtls`) carga certificados
    /// **ML-DSA** (firma post-cuántica) end to end — el runtime usa el provider
    /// PQC. Se salta si openssl no soporta ML-DSA (necesita 3.5+).
    #[test]
    fn public_api_loads_ml_dsa_certs_needs_openssl35() {
        let dir = std::env::temp_dir().join(format!("tls_mldsa_api_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = |f: &str| dir.join(f).to_str().unwrap().to_string();
        let key = |f: &str| {
            openssl(&[
                "genpkey",
                "-algorithm",
                "ML-DSA-65",
                "-provparam",
                "ml-dsa.output_formats=seed-only",
                "-out",
                &p(f),
            ])
        };
        if !key("ca.key")
            || !openssl(&[
                "req",
                "-x509",
                "-key",
                &p("ca.key"),
                "-out",
                &p("ca.crt"),
                "-days",
                "2",
                "-subj",
                "/CN=ca",
                "-addext",
                "basicConstraints=critical,CA:TRUE",
            ])
        {
            crate::test_support::skip_or_fail(
                "openssl sin ML-DSA (<3.5): no se pueden emitir los certs del test",
            );
            return;
        }
        for (n, eku) in [("srv", "serverAuth"), ("cli", "clientAuth")] {
            assert!(key(&format!("{n}.key")));
            assert!(openssl(&[
                "req",
                "-new",
                "-key",
                &p(&format!("{n}.key")),
                "-out",
                &p(&format!("{n}.csr")),
                "-subj",
                &format!("/CN={n}"),
            ]));
            let ext = dir.join(format!("{n}.ext"));
            std::fs::write(
                &ext,
                format!("subjectAltName=DNS:localhost\nextendedKeyUsage={eku}\n"),
            )
            .unwrap();
            assert!(openssl(&[
                "x509",
                "-req",
                "-in",
                &p(&format!("{n}.csr")),
                "-CA",
                &p("ca.crt"),
                "-CAkey",
                &p("ca.key"),
                "-CAcreateserial",
                "-days",
                "2",
                "-out",
                &p(&format!("{n}.crt")),
                "-extfile",
                ext.to_str().unwrap(),
            ]));
        }

        // Las funciones públicas cargan y construyen las configs con certs ML-DSA.
        server_config(
            &dir.join("srv.crt"),
            &dir.join("srv.key"),
            Some(&dir.join("ca.crt")),
        )
        .expect("server_config con cert ML-DSA");
        client_config_mtls(
            Some(&dir.join("ca.crt")),
            &dir.join("cli.crt"),
            &dir.join("cli.key"),
        )
        .expect("client_config_mtls con cert ML-DSA");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
