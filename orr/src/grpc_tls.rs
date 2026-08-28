//! TLS del gRPC del ORR: identidad de servidor (mTLS con la CA de red) y
//! canales de cliente hacia los pares.
//!
//! Por ese gRPC viaja el material de transporte del DKMS, así que va con mTLS
//! **por defecto** (`grpc_tls`). En claro sólo si alguien lo escribe
//! (`grpc_tls = false`), y eso vale únicamente cuando DKMS y ORR comparten
//! máquina o red interna de confianza.
//!
//! La configuración se guarda una vez en un `OnceLock`: los tres sitios que
//! abren canales hacia pares (bootstrap ×2, rotación) están lejos del `main`
//! y pasarles la config a mano tocaría media docena de firmas para lo mismo.

use std::sync::OnceLock;

use common::http::ControlTlsCfg;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Identity, ServerTlsConfig};

/// (identidad, `grpc_tls`).
static TLS: OnceLock<(Option<ControlTlsCfg>, bool)> = OnceLock::new();

/// Fija la identidad TLS del proceso y si el gRPC va con mTLS. Llamar una
/// vez, desde `main`.
pub fn install(cfg: Option<ControlTlsCfg>, enabled: bool) {
    let _ = TLS.set((cfg, enabled));
}

fn cfg() -> Option<&'static ControlTlsCfg> {
    TLS.get().and_then(|(c, _)| c.as_ref())
}

/// `true` si el gRPC de este ORR va con mTLS (y por tanto los pares también).
pub fn enabled() -> bool {
    TLS.get().map(|(_, e)| *e).unwrap_or(false)
}

/// URL con la que se diala a un par. Con `grpc_tls`, `https://` diga lo que
/// diga la SDN o el TOML —ninguno de los dos sabe de TLS, y el esquema es
/// una decisión de despliegue, la misma en todos los ORR—; si no, tal cual.
pub fn peer_url(url: &str) -> String {
    let u = url.trim();
    if enabled() && u.len() >= 7 && u[..7].eq_ignore_ascii_case("http://") {
        return format!("https://{}", &u[7..]);
    }
    u.to_string()
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

/// Canal hacia un par. Con `grpc_tls` (o una URL `https://` explícita)
/// presenta la identidad de este ORR y verifica al par con la CA de red; en
/// claro sólo si `grpc_tls = false` y la URL es `http://`. Una URL `https://`
/// sin identidad configurada es un error, no un silencio: es exactamente la
/// config asimétrica que hay que ver.
pub async fn channel(url: &str) -> Result<Channel, String> {
    let url = peer_url(url);
    let mut ep = Channel::from_shared(url.clone()).map_err(|e| format!("addr inválido: {e}"))?;
    if url.to_ascii_lowercase().starts_with("https://") {
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
    // Cadena de causas entera: "transport error" a secas no dice si es TLS
    // (cert de otra CA, SAN que no casa) o red.
    ep.connect()
        .await
        .map_err(|e| format!("connect: {:#}", anyhow::Error::new(e)))
}

#[cfg(test)]
mod tests {
    use super::peer_url;

    #[test]
    fn peer_url_keeps_scheme_when_plaintext() {
        // Sin `install`, `enabled()` es false: la URL se respeta tal cual.
        assert_eq!(peer_url("http://10.0.0.2:20003"), "http://10.0.0.2:20003");
        assert_eq!(peer_url("https://10.0.0.2:20003"), "https://10.0.0.2:20003");
    }
}
