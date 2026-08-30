//! Cliente HTTP compartido para los *announcers* al SDN.
//!
//! El plano de anuncio/heartbeat cruza instituciones. Cuando el SDN corre
//! con mTLS (`sdn_http_url = https://…`), cada módulo debe presentar su cert
//! de la CA de red; en claro (`http://…`) va sin cert, como hasta ahora.
//! Este helper centraliza esa decisión para dkms/orr/qkc y evita duplicar el
//! patrón `use_rustls_tls()+Identity+add_root_certificate` (el que ya usa
//! `dkms/src/peer_client.rs`; NO usar `use_preconfigured_tls`, ver su nota).

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use reqwest::{Certificate, Client, Identity};
use serde::{Deserialize, Serialize};

/// Material TLS de cliente para hablar mTLS con el SDN.
#[derive(Clone, Debug)]
pub struct ClientTls<'a> {
    /// CA de red (verifica el cert servidor del SDN). Puede ser un bundle.
    pub ca_path: &'a Path,
    /// Cert de este módulo (firmado por la CA de red).
    pub cert_path: &'a Path,
    /// Clave privada del cert.
    pub key_path: &'a Path,
}

/// Sección `[tls]` de config para módulos de control (orr/qkc) que se anuncian
/// al SDN. Owned (deserializable); `as_client_tls` la presta para el builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlTlsCfg {
    /// Cert de este módulo, firmado por la CA de red.
    pub cert_path: PathBuf,
    /// Clave privada del cert.
    pub key_path: PathBuf,
    /// CA de red: verifica el cert servidor del SDN.
    pub control_plane_ca: PathBuf,
}

impl ControlTlsCfg {
    pub fn as_client_tls(&self) -> ClientTls<'_> {
        ClientTls {
            ca_path: &self.control_plane_ca,
            cert_path: &self.cert_path,
            key_path: &self.key_path,
        }
    }
}

/// Construye el cliente reqwest para un announcer.
///
/// - `url` empieza por `https` **y** `tls` es `Some` → cliente mTLS.
/// - en otro caso → cliente en claro (comportamiento histórico).
///
/// Así el esquema del `sdn_http_url` decide el transporte sin flags extra.
pub fn announcer_client(
    url: &str,
    tls: Option<ClientTls<'_>>,
    timeout: Duration,
) -> anyhow::Result<Client> {
    let base = Client::builder().timeout(timeout);
    if !url_is_https(url) {
        return Ok(base.build()?);
    }
    let Some(tls) = tls else {
        anyhow::bail!("sdn_http_url is https but no [tls] client material configured");
    };
    let identity = load_identity(tls.cert_path, tls.key_path)?;
    let mut builder = base.use_rustls_tls().identity(identity).https_only(true);
    for ca in load_ca_bundle(tls.ca_path)? {
        builder = builder.add_root_certificate(ca);
    }
    Ok(builder.build()?)
}

/// `true` si el URL usa el esquema https (case-insensitive).
pub fn url_is_https(url: &str) -> bool {
    let u = url.trim_start();
    u.len() >= 5
        && u[..u.len().min(8)]
            .to_ascii_lowercase()
            .starts_with("https://")
}

fn load_identity(cert_path: &Path, key_path: &Path) -> anyhow::Result<Identity> {
    let cert = std::fs::read(cert_path)?;
    let key = std::fs::read(key_path)?;
    let mut bundle = cert;
    if !bundle.ends_with(b"\n") {
        bundle.push(b'\n');
    }
    bundle.extend_from_slice(&key);
    Ok(Identity::from_pem(&bundle)?)
}

/// reqwest no acepta multi-PEM en un `Certificate`; separa el bundle.
fn load_ca_bundle(ca_path: &Path) -> anyhow::Result<Vec<Certificate>> {
    let pem = std::fs::read(ca_path)?;
    let text = std::str::from_utf8(&pem)?;
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.lines() {
        current.push_str(line);
        current.push('\n');
        if line.starts_with("-----END CERTIFICATE-----") {
            out.push(Certificate::from_pem(current.as_bytes())?);
            current.clear();
        }
    }
    if out.is_empty() {
        anyhow::bail!("ca file {} has no CERTIFICATE blocks", ca_path.display());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_decides_transport() {
        assert!(url_is_https("https://sdn:8081"));
        assert!(url_is_https("HTTPS://sdn:8081"));
        assert!(!url_is_https("http://sdn:8081"));
        assert!(!url_is_https("sdn:8081"));
    }

    #[test]
    fn plaintext_url_needs_no_tls() {
        let c = announcer_client("http://sdn:8081", None, Duration::from_secs(5));
        assert!(c.is_ok());
    }

    #[test]
    fn https_without_material_errors() {
        let c = announcer_client("https://sdn:8081", None, Duration::from_secs(5));
        assert!(c.is_err());
    }
}
