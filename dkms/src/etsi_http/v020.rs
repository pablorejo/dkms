//! Handlers ETSI GS QKD 020 (DKMS↔DKMS).
//!
//! Endpoints servidos:
//!
//! * `POST /kmapi/v1/ext_keys`     — recibe claves desde otro DKMS.
//! * `POST /kmapi/v1/ext_keys/ack` — recibe ACK asíncrono (cuando el
//!   peer responde con `ack_callback_url`). Por ahora solo loggea, el
//!   flujo síncrono cubre la entrega y el reembolso de tokens.
//! * `GET  /kmapi/v1/versions`     — versión del API ETSI 020.

use axum::{
    extract::{Json, State},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use tracing::{info, instrument};

use etsi::v020::{Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer, Etsi020VersionContainer};

use crate::{etsi_http::auth::DkmsPeer, service::DkmsService};

pub fn router(svc: DkmsService) -> Router {
    Router::new()
        .route("/kmapi/v1/ext_keys", post(handle_ext_keys))
        .route("/kmapi/v1/ext_keys/ack", post(handle_ext_keys_ack))
        .route("/kmapi/v1/versions", get(handle_versions))
        .with_state(svc)
}

#[instrument(skip(svc, peer, body))]
async fn handle_ext_keys(
    State(svc): State<DkmsService>,
    peer: DkmsPeer,
    Json(body): Json<Etsi020ExtKeyContainer>,
) -> Response {
    match svc.handle_incoming_ext_keys(&peer.node_id, body).await {
        Ok(ack) => Json(ack).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(_svc, peer, ack))]
async fn handle_ext_keys_ack(
    State(_svc): State<DkmsService>,
    peer: DkmsPeer,
    Json(ack): Json<Etsi020ExtKeyAckContainer>,
) -> Response {
    // Hoy el flujo síncrono cubre el reembolso de tokens. Loggeamos por si
    // un peer prefiere ACK diferido.
    info!(
        peer = %peer.node_id,
        ack_status = ?ack.ack_status,
        keys = ack.key_ids.len(),
        "received deferred ETSI 020 ack"
    );
    Json(serde_json::json!({"status": "ok"})).into_response()
}

#[instrument(skip(_svc))]
async fn handle_versions(State(_svc): State<DkmsService>) -> Response {
    let v = Etsi020VersionContainer::new(vec!["1.0".to_owned()]);
    Json(v).into_response()
}
