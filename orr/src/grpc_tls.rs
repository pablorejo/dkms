//! TLS del gRPC del ORR: identidad de servidor (mTLS con la CA de red) y
//! canales de cliente hacia los pares.
//!
//! Sin esto, el gRPC DKMS↔ORR y ORR↔ORR va en claro, y por el primero viaja
//! el material de transporte sin cifrar. Es correcto solo si los dos módulos
//! comparten máquina o red interna de confianza; si no, `grpc_tls = true`.
//!
//! La configuración se guarda una vez en un `OnceLock`: los tres sitios que
//! abren canales hacia pares (bootstrap ×2, rotación) están lejos del `main`
//! y pasarles la config a mano tocaría media docena de firmas para lo mismo.

use std::sync::OnceLock;

use common::http::ControlTlsCfg;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, ServerTlsConfig};

static TLS: OnceLock<Option<ControlTlsCfg>> = OnceLock::new();

/// Fija la identidad TLS del proceso. Llamar una vez, desde `main`.
pub fn install(cfg: Option<ControlTlsCfg>) {
    let _ = TLS.set(cfg);
}

fn cfg() -> Option<&'static ControlTlsCfg> {
    TLS.get().and_then(|o| o.as_ref())
}

/// `ServerTlsConfig` para el gRPC de este ORR: presenta su cert y **exige**
/// cert de cliente de la CA de red. `None` si no hay identidad configurada.
pub fn server() -> anyhow::Result<Option<ServerTlsConfig>> {
    let Some(t) = cfg() else { return Ok(None) };
    let cert = std::fs::read(&t.cert_path)?;
    let key = std::fs::read(&t.key_path)?;
    let ca = std::fs::read(&t.control_plane_ca)?;
    Ok(Some(
        ServerTlsConfig::new()
            .identity(Identity::from_pem(cert, key))
            .client_ca_root(Certificate::from_pem(ca)),
    ))
}

/// Canal hacia un par. Si la URL es `https://` presenta la identidad de este
/// ORR y verifica al par con la CA de red; si es `http://` va en claro, como
/// siempre. Una URL `https://` sin identidad configurada es un error, no un
/// silencio: es exactamente la config asimétrica que hay que ver.
pub async fn channel(url: &str) -> Result<Channel, String> {
    let mut ep =
        Channel::from_shared(url.to_string()).map_err(|e| format!("addr inválido: {e}"))?;
    if url
        .trim_start()
        .to_ascii_lowercase()
        .starts_with("https://")
    {
        let t = cfg().ok_or_else(|| {
            format!("{url}: el par exige TLS pero este ORR no tiene [tls] configurado")
        })?;
        let cert = std::fs::read(&t.cert_path).map_err(|e| format!("tls.cert_path: {e}"))?;
        let key = std::fs::read(&t.key_path).map_err(|e| format!("tls.key_path: {e}"))?;
        let ca =
            std::fs::read(&t.control_plane_ca).map_err(|e| format!("tls.control_plane_ca: {e}"))?;
        ep = ep
            .tls_config(
                ClientTlsConfig::new()
                    .ca_certificate(Certificate::from_pem(ca))
                    .identity(Identity::from_pem(cert, key)),
            )
            .map_err(|e| format!("tls: {e}"))?;
    }
    ep.connect().await.map_err(|e| format!("connect: {e}"))
}
