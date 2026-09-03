//! Handlers ETSI GS QKD 020 (DKMS↔DKMS).
//!
//! Endpoints servidos:
//!
//! * `POST /kmapi/v1/ext_keys`     — recibe claves desde otro DKMS.
//! * `POST /kmapi/v1/ext_keys/ack` — recibe ACK asíncrono (cuando el
//!   peer responde con `ack_callback_url`). Por ahora solo loggea, el
//!   flujo síncrono cubre la entrega y el reembolso de tokens.
//! * `GET  /kmapi/v1/versions`     — versión del API ETSI 020.
//! * `POST /kmapi/v1/e2e/kem`      — acuerdo de clave de la capa extremo a
//!   extremo DKMS↔DKMS ([`crate::e2e`]). No es ETSI: es nuestro, pero vive
//!   en este plano porque es el que ya autentica a los DKMS entre sí.

use axum::{
    extract::{Json, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tracing::{debug, instrument};

use etsi::v020::{Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer, Etsi020VersionContainer};

use crate::{etsi_http::auth::DkmsPeer, service::DkmsService};

pub fn router(svc: DkmsService) -> Router {
    Router::new()
        .route("/kmapi/v1/ext_keys", post(handle_ext_keys))
        .route("/kmapi/v1/ext_keys/ack", post(handle_ext_keys_ack))
        .route("/kmapi/v1/versions", get(handle_versions))
        .route(crate::e2e::KEM_PATH, post(handle_e2e_kem))
        .with_state(svc)
}

#[instrument(skip(svc, peer, body))]
async fn handle_ext_keys(
    State(svc): State<DkmsService>,
    peer: DkmsPeer,
    Json(body): Json<Etsi020ExtKeyContainer>,
) -> Response {
    if let Err(e) = svc.require_known_peer(&peer.node_id) {
        return super::error_to_response(e);
    }
    match svc.handle_incoming_ext_keys(&peer.node_id, body).await {
        Ok(ack) => Json(ack).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(svc, peer, ack))]
async fn handle_ext_keys_ack(
    State(svc): State<DkmsService>,
    peer: DkmsPeer,
    Json(ack): Json<Etsi020ExtKeyAckContainer>,
) -> Response {
    if let Err(e) = svc.require_known_peer(&peer.node_id) {
        return super::error_to_response(e);
    }
    // ACK autenticado por mTLS: la identidad del emisor es su cert
    // (`peer.node_id`), no un campo del cuerpo. Movemos las claves de
    // `ack_pending` a `buffer_enc` (docs/SECURITY.md §Fase 4). Es la variante
    // segura del socket TCP plano heredado; hoy los ACKs salientes aún usan el
    // socket, así que esta ruta solo actúa si un peer decide usarla.
    let matched = svc.handle_incoming_ack(&peer.node_id, &ack.key_ids);
    // Un lote cada 50 ms por peer bajo carga: contador en `generator.state`
    // (`ack_recv`), línea a debug.
    svc.flow
        .peer(peer.node_id.as_str())
        .ack_recv
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    debug!(
        peer = %peer.node_id,
        ack_status = ?ack.ack_status,
        keys = ack.key_ids.len(),
        matched,
        "received ETSI 020 ack"
    );
    Json(serde_json::json!({"status": "ok", "matched": matched})).into_response()
}

/// El peer que pide es el del certificado (`peer.node_id`); el cuerpo sólo
/// lleva su pública efímera. Respondemos con la época que le asignamos y
/// pasamos a emitirle con ella.
#[instrument(skip(svc, peer, req))]
async fn handle_e2e_kem(
    State(svc): State<DkmsService>,
    peer: DkmsPeer,
    Json(req): Json<crate::e2e::KemRequest>,
) -> Response {
    if let Err(e) = svc.require_known_peer(&peer.node_id) {
        return super::error_to_response(e);
    }
    match svc.e2e.respond(peer.node_id.as_str(), &req) {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => super::error_to_response(crate::error::DkmsError::BadRequest(e.to_string())),
    }
}

#[instrument(skip(_svc))]
async fn handle_versions(State(_svc): State<DkmsService>) -> Response {
    let v = Etsi020VersionContainer::new(vec!["1.0".to_owned()]);
    Json(v).into_response()
}
