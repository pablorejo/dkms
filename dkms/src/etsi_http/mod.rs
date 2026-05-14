//! Servidor HTTP del DKMS.
//!
//! Dos planos en puertos separados con sus propios CAs cliente:
//!
//! * **SAE plane** (`listen.sae_addr`) — sirve ETSI 014 (`v014`).
//! * **Peer plane** (`listen.peer_addr`) — sirve ETSI 020 (`v020`).
//!
//! Ambos comparten el mismo cert servidor del DKMS y se sirven sobre
//! HTTP/2 + mTLS. La verificación de cliente la hace `rustls`; la
//! identidad (SAE o DKMS) se decodifica del SAN URI por el módulo
//! [`auth`].

pub mod admission_layer;
pub mod auth;
pub mod mtls;
pub mod v014;
pub mod v020;

use std::sync::Arc;

use anyhow::Result;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;
use tracing::warn;

use crate::{config::ListenCfg, error::DkmsError, service::DkmsService};
use admission_layer::AdmissionLayer;

/// Lanza los dos servidores HTTPS (SAE plane + Peer plane).
///
/// Cada servidor corre en su propio task; el future devuelto se completa
/// cuando cualquiera de los dos falle.
pub async fn serve(
    svc: DkmsService,
    listen: &ListenCfg,
    sae_tls: Arc<rustls::ServerConfig>,
    peer_tls: Arc<rustls::ServerConfig>,
) -> Result<()> {
    let admission_layer = AdmissionLayer::new(svc.admission.clone());
    let sae_router = v014::router(svc.clone())
        .route("/healthz", get(healthz))
        .layer(admission_layer.clone());
    let peer_router = v020::router(svc.clone())
        .route("/healthz", get(healthz))
        .layer(admission_layer);

    let sae_task = tokio::spawn(mtls::serve_mtls(
        listen.sae_addr,
        sae_tls,
        sae_router,
        "sae",
    ));
    let peer_task = tokio::spawn(mtls::serve_mtls(
        listen.peer_addr,
        peer_tls,
        peer_router,
        "peer-dkms",
    ));

    tokio::select! {
        r = sae_task  => r??,
        r = peer_task => r??,
    }
    Ok(())
}

async fn healthz() -> Response {
    (StatusCode::OK, axum::Json(json!({"status":"ok"}))).into_response()
}

/// Mapea [`DkmsError`] a una respuesta HTTP coherente con ETSI 014/020.
///
/// Decisión clave para no filtrar metadatos: tanto "clave inexistente"
/// como "SAE no autorizado" devuelven el mismo 404, así un atacante no
/// puede sondear `key_id`s buscando errores discriminantes.
pub fn error_to_response(err: DkmsError) -> Response {
    let (status, msg) = match &err {
        DkmsError::Unauthenticated => (StatusCode::UNAUTHORIZED, err.to_string()),
        DkmsError::Forbidden(..) => (StatusCode::FORBIDDEN, err.to_string()),
        DkmsError::UnknownSae(_) => (StatusCode::NOT_FOUND, err.to_string()),
        DkmsError::BadRequest(_) => (StatusCode::BAD_REQUEST, err.to_string()),
        DkmsError::RateLimited { .. } => (StatusCode::TOO_MANY_REQUESTS, err.to_string()),
        DkmsError::KeyNotAuthorized { .. } | DkmsError::KeyExpired => {
            (StatusCode::NOT_FOUND, "key not found".to_owned())
        }
        DkmsError::TransportBufferEmpty { .. } | DkmsError::TransportKeyMissing { .. } => {
            (StatusCode::SERVICE_UNAVAILABLE, err.to_string())
        }
        DkmsError::PeerUnreachable { .. }
        | DkmsError::PeerRejected { .. }
        | DkmsError::PeerAckTimeout { .. }
        | DkmsError::SdnUnreachable(_)
        | DkmsError::QkcUnreachable(_) => (StatusCode::BAD_GATEWAY, err.to_string()),
        DkmsError::SaeBindingLookupFailed(_) => (StatusCode::NOT_FOUND, err.to_string()),
        DkmsError::Tls(_)
        | DkmsError::Crypto(_)
        | DkmsError::Io(_)
        | DkmsError::Common(_)
        | DkmsError::Other(_) => {
            warn!(error = ?err, "internal dkms error");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_owned())
        }
    };
    (status, axum::Json(json!({"message": msg}))).into_response()
}
