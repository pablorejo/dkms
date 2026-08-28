//! Cliente HTTP/2 + mTLS hacia otros DKMS para entregar ETSI 020.
//!
//! Diseño:
//!
//! * Una sola instancia `reqwest::Client` reutilizada para todos los peers
//!   (reqwest hace pooling por host internamente).
//! * Usamos la API de alto nivel de reqwest (`identity` +
//!   `add_root_certificate`) en vez de `use_preconfigured_tls`, porque
//!   esta última falla con `"Unknown TLS backend"` si nuestra crate de
//!   `rustls` no coincide *exactamente* con la versión interna que
//!   bundle-a reqwest. La API de alto nivel sigue dando mTLS rustls real
//!   (reqwest pasa los bytes PEM por dentro).
//! * Sin `http2_prior_knowledge()`: el handshake negocia h2 vía ALPN
//!   normal contra el server (que anuncia `h2`/`http/1.1`).
//! * Timeouts apropiados a cada llamada (configurables vía
//!   [`crate::config::RequestCfg`]).
//! * Endpoint final: `{peer.endpoint}/kmapi/v1/ext_keys` (alineado con el
//!   `Etsi020PostExtKeys::get_endpoint_url` del crate `etsi`).

use std::{fs, path::Path, sync::Arc, time::Duration};

use anyhow::anyhow;
use reqwest::{Certificate, Client, Identity};
use tracing::debug;

use etsi::v020::{
    ack_status::Etsi020AckStatus, Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer,
    Etsi020KeyID,
};

use crate::{
    config::{PeerCfg, RequestCfg},
    error::{DkmsError, Result},
};

/// Cliente reutilizable hacia cualquier peer DKMS.
#[derive(Clone)]
pub struct PeerHttpClient {
    inner: Client,
    request: RequestCfg,
}

impl PeerHttpClient {
    /// Construye el cliente cargando cert/key locales y la CA de peers
    /// desde disco. Se llama una vez al arrancar el DKMS.
    pub fn build(
        dkms_cert: &Path,
        dkms_key: &Path,
        peer_ca: &Path,
        request: RequestCfg,
    ) -> Result<Self> {
        let cert_pem = fs::read(dkms_cert).map_err(|e| {
            DkmsError::Crypto(format!("read dkms cert {}: {e}", dkms_cert.display()))
        })?;
        let key_pem = fs::read(dkms_key)
            .map_err(|e| DkmsError::Crypto(format!("read dkms key {}: {e}", dkms_key.display())))?;
        let ca_pem = fs::read(peer_ca)
            .map_err(|e| DkmsError::Crypto(format!("read peer_ca {}: {e}", peer_ca.display())))?;

        // reqwest::Identity::from_pem espera cert+key concatenados.
        let mut bundle = Vec::with_capacity(cert_pem.len() + key_pem.len() + 1);
        bundle.extend_from_slice(&cert_pem);
        if !cert_pem.ends_with(b"\n") {
            bundle.push(b'\n');
        }
        bundle.extend_from_slice(&key_pem);

        let identity = Identity::from_pem(&bundle)
            .map_err(|e| DkmsError::Crypto(format!("identity from pem: {e}")))?;

        // No usamos http2_prior_knowledge: con esa opción + use_rustls_tls
        // reqwest 0.12 envía bytes h2 sin completar correctamente el ciclo
        // ALPN sobre rustls — el servidor rustls reporta `received corrupt
        // message of type InvalidContentType` y aborta. Dejando que h2 se
        // negocie por ALPN normal (server.alpn_protocols = ["h2","http/1.1"]
        // en common::tls::server_config), reqwest hace handshake limpio y
        // sigue usando HTTP/2 al ser h2 el primer protocol ofrecido.
        let mut builder = Client::builder()
            .use_rustls_tls()
            .identity(identity)
            .https_only(true)
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_millis(request.peer_send_timeout_ms))
            .timeout(Duration::from_millis(
                request
                    .peer_send_timeout_ms
                    .saturating_add(request.ack_wait_timeout_ms),
            ));

        // Cada cert PEM dentro del bundle de CA va por separado a
        // `add_root_certificate` (reqwest no acepta multi-PEM en un único
        // Certificate). El cert de runtime CA y la chain externa van todos.
        for ca_cert in parse_ca_bundle(&ca_pem)? {
            builder = builder.add_root_certificate(ca_cert);
        }

        let client = builder.build().map_err(|e| DkmsError::Other(anyhow!(e)))?;

        debug!("peer http client ready (HTTP/2 + mTLS via reqwest::rustls)");
        Ok(Self {
            inner: client,
            request,
        })
    }

    /// POST síncrono del ETSI 020 a un peer. Devuelve el ACK que devuelve el
    /// peer en el cuerpo de la respuesta.
    pub async fn send_ext_keys(
        &self,
        peer_node: &str,
        peer: &PeerCfg,
        body: &Etsi020ExtKeyContainer,
    ) -> Result<Etsi020ExtKeyAckContainer> {
        let url = format!("{}/kmapi/v1/ext_keys", peer.endpoint.trim_end_matches('/'));
        let req = self.inner.post(&url).json(body);

        let resp = req.send().await.map_err(|e| DkmsError::PeerUnreachable {
            peer: peer_node.to_owned(),
            source: anyhow!(e),
        })?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(DkmsError::PeerRejected {
                peer: peer_node.to_owned(),
                status: status.as_u16(),
                body,
            });
        }

        let ack: Etsi020ExtKeyAckContainer =
            resp.json().await.map_err(|e| DkmsError::PeerRejected {
                peer: peer_node.to_owned(),
                status: status.as_u16(),
                body: format!("ack parse: {e}"),
            })?;
        Ok(ack)
    }

    /// POST de un ACK por el plano ETSI-020 (mTLS). Devuelve cuántos `key_ids`
    /// dice el peer que casaron.
    ///
    /// Es la variante **autenticada** del ACK: la identidad del emisor la pone
    /// el certificado de cliente, no un campo del cuerpo. El socket TCP heredado
    /// acepta conexiones de cualquiera y se cree el `from` que le manden, así
    /// que un ACK forjado saca entradas de `ack_pending` y descuadra el
    /// generador (docs/SECURITY.md §Fase 4).
    ///
    /// El receptor ya estaba: `handle_ext_keys_ack` → `handle_incoming_ack`.
    /// Esto es sólo el lado emisor, que faltaba.
    pub async fn send_ext_keys_ack(
        &self,
        peer_node: &str,
        peer: &PeerCfg,
        key_ids: &[String],
    ) -> Result<usize> {
        let url = format!(
            "{}/kmapi/v1/ext_keys/ack",
            peer.endpoint.trim_end_matches('/')
        );
        let body = Etsi020ExtKeyAckContainer {
            key_ids: key_ids
                .iter()
                .filter_map(|k| k.parse().ok().map(Etsi020KeyID::new))
                .collect(),
            ack_status: Etsi020AckStatus::Relayed,
            initiator_sae_id: Default::default(),
            target_sae_id: Default::default(),
            message: None,
            extension: None,
        };
        let resp = self
            .inner
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| DkmsError::PeerUnreachable {
                peer: peer_node.to_owned(),
                source: anyhow!(e),
            })?;
        let status = resp.status();
        if !status.is_success() {
            let b = resp.text().await.unwrap_or_default();
            return Err(DkmsError::PeerRejected {
                peer: peer_node.to_owned(),
                status: status.as_u16(),
                body: b,
            });
        }
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        Ok(v.get("matched").and_then(|m| m.as_u64()).unwrap_or(0) as usize)
    }

    pub fn request_cfg(&self) -> &RequestCfg {
        &self.request
    }
}

/// Divide un PEM con potencialmente varios `BEGIN CERTIFICATE` y los
/// devuelve como `reqwest::Certificate` independientes (la API de
/// reqwest no acepta multi-PEM concatenado).
fn parse_ca_bundle(pem: &[u8]) -> Result<Vec<Certificate>> {
    let mut out = Vec::new();
    let text = std::str::from_utf8(pem)
        .map_err(|e| DkmsError::Crypto(format!("peer_ca not valid utf-8: {e}")))?;
    let mut current = String::new();
    for line in text.lines() {
        current.push_str(line);
        current.push('\n');
        if line.starts_with("-----END CERTIFICATE-----") {
            let cert = Certificate::from_pem(current.as_bytes())
                .map_err(|e| DkmsError::Crypto(format!("peer_ca cert parse: {e}")))?;
            out.push(cert);
            current.clear();
        }
    }
    if out.is_empty() {
        return Err(DkmsError::Crypto(
            "peer_ca file has no BEGIN CERTIFICATE blocks".into(),
        ));
    }
    Ok(out)
}

// Mantenemos este import sólo para que documentación/tests legados
// resuelvan el path. Si en el futuro se borra, eliminar también el
// `use Arc` superfluo.
#[allow(dead_code)]
fn _arc_keepalive(_: &Arc<()>) {}
