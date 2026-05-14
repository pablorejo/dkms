//! Handlers ETSI GS QKD 014 (SAE-facing).
//!
//! Tres endpoints clásicos:
//!
//! * `GET  /api/v1/keys/{slave_SAE_ID}/status`
//! * `POST /api/v1/keys/{slave_SAE_ID}/enc_keys`
//! * `POST /api/v1/keys/{master_SAE_ID}/dec_keys`
//!
//! La autenticación se inyecta vía [`SaePeer`] (extractor mTLS). El cuerpo
//! del request lo deserializa axum con `axum::Json` usando los tipos del
//! crate `etsi`. La traducción a HTTP de los errores vive en [`super::error_to_response`].

use axum::{
    extract::{Json, Path, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use serde_json::Value;
use tracing::instrument;

use common::ids::SaeId;
use etsi::v014::{Etsi014KeyIDs, Etsi014KeyRequest};

use crate::{service::DkmsService, etsi_http::auth::SaePeer};

pub fn router(svc: DkmsService) -> Router {
    Router::new()
        .route(
            "/api/v1/keys/:slave_sae/status",
            get(handle_status),
        )
        .route(
            "/api/v1/keys/:slave_sae/enc_keys",
            post(handle_enc_keys),
        )
        .route(
            "/api/v1/keys/:master_sae/dec_keys",
            post(handle_dec_keys),
        )
        .with_state(svc)
}

#[instrument(skip(svc, peer))]
async fn handle_status(
    State(svc): State<DkmsService>,
    Path(slave_sae): Path<String>,
    peer: SaePeer,
) -> Response {
    let slave = SaeId::new(slave_sae);
    match svc.status_for(&peer.sae_id, &slave).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(svc, peer, headers, body))]
async fn handle_enc_keys(
    State(svc): State<DkmsService>,
    Path(slave_sae): Path<String>,
    peer: SaePeer,
    headers: HeaderMap,
    Json(body): Json<Etsi014KeyRequest>,
) -> Response {
    let slave = SaeId::new(slave_sae);
    let extra_saes_header = headers
        .get("additional-saes")
        .or_else(|| headers.get("X-Additional-Saes"))
        .and_then(|v| v.to_str().ok())
        .map(|s| Value::String(s.to_owned()));

    match svc
        .handle_enc_keys(&peer.sae_id, &slave, body, extra_saes_header.as_ref())
        .await
    {
        Ok(c) => Json(c).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(svc, peer, body))]
async fn handle_dec_keys(
    State(svc): State<DkmsService>,
    Path(master_sae): Path<String>,
    peer: SaePeer,
    Json(body): Json<Etsi014KeyIDs>,
) -> Response {
    let master = SaeId::new(master_sae);
    match svc.handle_dec_keys(&peer.sae_id, &master, body).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => super::error_to_response(e),
    }
}
