//! Servidor HTTP que expone los endpoints ETSI GS QKD 014.
//!
//! Soporta dos serializaciones — el cliente negocia con `Accept`:
//!
//! * `Accept: application/json` (default) → ETSI 014 estándar
//!   (JSON + base64). Compatible con cualquier KME ortodoxo.
//! * `Accept: application/octet-stream` → wire binario interno (ver
//!   `etsi::binary`). Ahorra base64 + JSON parsing → throughput
//!   bastante mayor para batches grandes.
//!
//! Endpoints:
//!
//! ```text
//!   GET  /healthz                                        liveness
//!   GET  /api/v1/keys/{sae_id}/status                    Etsi014Status (JSON)
//!   GET  /api/v1/keys/{sae_id}/enc_keys?number=N&size=B  JSON o binario
//!   GET  /api/v1/keys/{sae_id}/dec_keys?key_ID=<uuid>    JSON o binario
//!   POST /api/v1/keys/{sae_id}/dec_keys                  JSON o binario
//! ```

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use etsi::{
    binary,
    v014::{
        Etsi014Error, Etsi014Key, Etsi014KeyContainer, Etsi014KeyIDs, Etsi014Status,
    },
    Base64Bytes,
};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::{info, warn};
use uuid::Uuid;

use crate::service::QudittoService;

/// Construye el router con todos los endpoints.
pub fn router(svc: QudittoService) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/api/v1/keys/:sae_id/status", get(get_status))
        .route("/api/v1/keys/:sae_id/enc_keys", get(get_enc_keys))
        .route(
            "/api/v1/keys/:sae_id/dec_keys",
            get(get_dec_keys).post(post_dec_keys),
        )
        .with_state(svc)
}

pub async fn serve(svc: QudittoService, addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    info!(addr = %bound, "quditto HTTP ETSI 014 listening");
    axum::serve(listener, router(svc)).await?;
    Ok(())
}

// ─────────────────────────── helpers ─────────────────────────────

/// `true` si el cliente quiere wire binario (negociación por `Accept`).
fn wants_binary(headers: &HeaderMap) -> bool {
    let Some(accept) = headers.get(header::ACCEPT) else {
        return false;
    };
    let Ok(s) = accept.to_str() else { return false };
    s.contains(binary::CONTENT_TYPE)
}

/// `true` si el body viene en wire binario (negociación por `Content-Type`).
fn body_is_binary(headers: &HeaderMap) -> bool {
    let Some(ct) = headers.get(header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(s) = ct.to_str() else { return false };
    s.starts_with(binary::CONTENT_TYPE)
}

fn binary_response(body: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, binary::CONTENT_TYPE)],
        body,
    )
        .into_response()
}

// ─────────────────────────── handlers ────────────────────────────

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

async fn get_status(
    State(svc): State<QudittoService>,
    Path(sae_id): Path<String>,
) -> Response {
    let cfg = svc.cfg();
    let status = Etsi014Status {
        source_kme_id: "quditto".into(),
        target_kme_id: "quditto".into(),
        master_sae_id: "quditto-master".into(),
        slave_sae_id: sae_id,
        key_size: cfg.key_size_bits,
        stored_key_count: svc.link.fresh_available(),
        max_key_count: cfg.max_buffer_keys,
        max_key_per_request: 128,
        max_key_size: cfg.key_size_bits,
        min_key_size: cfg.key_size_bits,
        max_sae_id_count: 1,
        status_extension: None,
    };
    Json(status).into_response()
}

#[derive(Deserialize, Debug)]
struct EncQuery {
    #[serde(default = "default_number")]
    number: u32,
    #[serde(default = "default_size")]
    size: u32,
}
fn default_number() -> u32 {
    1
}
fn default_size() -> u32 {
    256
}

async fn get_enc_keys(
    State(svc): State<QudittoService>,
    Path(_sae_id): Path<String>,
    Query(q): Query<EncQuery>,
    headers: HeaderMap,
) -> Response {
    let cfg = svc.cfg();

    if q.size != cfg.key_size_bits {
        return etsi_error(
            StatusCode::BAD_REQUEST,
            format!("unsupported size {} (only {} supported)", q.size, cfg.key_size_bits),
        );
    }
    if q.number == 0 {
        return etsi_error(StatusCode::BAD_REQUEST, "number must be >= 1".into());
    }

    let taken = svc.link.take_for_enc(q.number as usize);
    if taken.len() < q.number as usize {
        warn!(
            requested = q.number,
            obtained = taken.len(),
            "quditto: enc_keys not enough fresh keys"
        );
        return etsi_error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!(
                "not enough fresh keys: requested {}, available {}",
                q.number,
                taken.len()
            ),
        );
    }

    if wants_binary(&headers) {
        let refs: Vec<(Uuid, &[u8])> = taken.iter().map(|k| (k.key_id, &k.material[..])).collect();
        return binary_response(binary::pack_keys(&refs, cfg.key_size_bits as u16));
    }

    let keys: Vec<Etsi014Key> = taken
        .into_iter()
        .map(|k| Etsi014Key {
            key_id: k.key_id,
            key_id_extension: None,
            key: Base64Bytes::new(k.material.to_vec()),
            key_extension: None,
        })
        .collect();

    Json(Etsi014KeyContainer { keys, key_container_extension: None }).into_response()
}

#[derive(Deserialize, Debug)]
struct DecQuery {
    #[serde(rename = "key_ID")]
    key_id: Uuid,
}

async fn get_dec_keys(
    State(svc): State<QudittoService>,
    Path(_sae_id): Path<String>,
    Query(q): Query<DecQuery>,
    headers: HeaderMap,
) -> Response {
    match svc.link.take_for_dec(&q.key_id) {
        Some(material) => {
            if wants_binary(&headers) {
                let mat: &[u8] = &material;
                let key_size_bits = (mat.len() * 8) as u16;
                return binary_response(binary::pack_keys(&[(q.key_id, mat)], key_size_bits));
            }
            Json(Etsi014KeyContainer {
                keys: vec![Etsi014Key {
                    key_id: q.key_id,
                    key_id_extension: None,
                    key: Base64Bytes::new(material.to_vec()),
                    key_extension: None,
                }],
                key_container_extension: None,
            })
            .into_response()
        }
        None => etsi_error(
            StatusCode::NOT_FOUND,
            format!("key_id {} not found (already consumed or never issued)", q.key_id),
        ),
    }
}

async fn post_dec_keys(
    State(svc): State<QudittoService>,
    Path(_sae_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Parsear body: binario o JSON según Content-Type.
    let ids: Vec<Uuid> = if body_is_binary(&headers) {
        match binary::unpack_key_ids(&body) {
            Ok(ids) => ids,
            Err(e) => return etsi_error(StatusCode::BAD_REQUEST, format!("bad binary body: {e}")),
        }
    } else {
        let parsed: Etsi014KeyIDs = match serde_json::from_slice(&body) {
            Ok(p) => p,
            Err(e) => return etsi_error(StatusCode::BAD_REQUEST, format!("bad json body: {e}")),
        };
        parsed.key_ids.into_iter().map(|k| k.key_id).collect()
    };

    if ids.is_empty() {
        return etsi_error(StatusCode::BAD_REQUEST, "key_IDs must be non-empty".into());
    }

    let mut materials: Vec<(Uuid, Vec<u8>)> = Vec::with_capacity(ids.len());
    let mut missing: Vec<String> = Vec::new();
    for id in &ids {
        match svc.link.take_for_dec(id) {
            Some(m) => materials.push((*id, m.to_vec())),
            None => missing.push(id.to_string()),
        }
    }

    if !missing.is_empty() {
        // ETSI 014 §6.2: si alguno falta, todo el batch falla.
        return etsi_error_with_details(
            StatusCode::NOT_FOUND,
            "one or more key_IDs not found".into(),
            missing,
        );
    }

    if wants_binary(&headers) {
        let cfg = svc.cfg();
        let refs: Vec<(Uuid, &[u8])> =
            materials.iter().map(|(id, m)| (*id, &m[..])).collect();
        return binary_response(binary::pack_keys(&refs, cfg.key_size_bits as u16));
    }

    let keys: Vec<Etsi014Key> = materials
        .into_iter()
        .map(|(id, m)| Etsi014Key {
            key_id: id,
            key_id_extension: None,
            key: Base64Bytes::new(m),
            key_extension: None,
        })
        .collect();
    Json(Etsi014KeyContainer { keys, key_container_extension: None }).into_response()
}

// ─────────────────────────── errors ──────────────────────────────

fn etsi_error(code: StatusCode, msg: String) -> Response {
    let body = Etsi014Error { message: msg, details: None };
    (code, Json(body)).into_response()
}

fn etsi_error_with_details(code: StatusCode, msg: String, details: Vec<String>) -> Response {
    use etsi::v014::error::Etsi014ErrorDetails;
    let body = Etsi014Error {
        message: msg,
        details: Some(Etsi014ErrorDetails::Strings(details)),
    };
    (code, Json(body)).into_response()
}
