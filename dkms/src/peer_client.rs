//! Cliente HTTP/2 + mTLS hacia otros DKMS para entregar ETSI 020.
//!
//! Diseño:
//!
//! * Una sola instancia `reqwest::Client` reutilizada para todos los peers
//!   (reqwest hace pooling por host internamente). Construida con
//!   `use_preconfigured_tls(rustls::ClientConfig)` para tener control total
//!   sobre la cadena de raíces y el cert cliente.
//! * `http2_prior_knowledge()` ⇒ sin ALPN dance ni upgrades.
//! * Timeouts apropiados a cada llamada (configurables vía
//!   [`crate::config::RequestCfg`]).
//! * Endpoint final: `{peer.endpoint}/kmapi/v1/ext_keys` (alineado con el
//!   `Etsi020PostExtKeys::get_endpoint_url` del crate `etsi`).

use std::{
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use reqwest::Client;
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    ClientConfig, RootCertStore,
};
use tracing::debug;

use etsi::v020::{Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer};

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
        let certs = common::tls::load_certs(dkms_cert)?;
        let key = common::tls::load_key(dkms_key)?;
        let cas = common::tls::load_certs(peer_ca)?;

        let tls = build_rustls_client_config(certs, key, cas)
            .map_err(|e| DkmsError::Crypto(format!("tls client config: {e}")))?;

        let client = Client::builder()
            .use_preconfigured_tls(tls)
            .http2_prior_knowledge()
            .pool_max_idle_per_host(64)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .connect_timeout(Duration::from_millis(request.peer_send_timeout_ms))
            .timeout(Duration::from_millis(
                request.peer_send_timeout_ms.saturating_add(request.ack_wait_timeout_ms),
            ))
            .build()
            .map_err(|e| DkmsError::Other(anyhow!(e)))?;

        debug!("peer http client ready (HTTP/2 + mTLS)");
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

        let ack: Etsi020ExtKeyAckContainer = resp.json().await.map_err(|e| DkmsError::PeerRejected {
            peer: peer_node.to_owned(),
            status: status.as_u16(),
            body: format!("ack parse: {e}"),
        })?;
        Ok(ack)
    }

    pub fn request_cfg(&self) -> &RequestCfg {
        &self.request
    }
}

fn build_rustls_client_config(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    cas: Vec<CertificateDer<'static>>,
) -> std::result::Result<Arc<ClientConfig>, rustls::Error> {
    let mut roots = RootCertStore::empty();
    for c in cas {
        roots.add(c)?;
    }
    let cfg = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(certs, key)?;
    Ok(Arc::new(cfg))
}
