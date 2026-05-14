//! Servidor HTTP que expone los endpoints ETSI GS QKD 014.
//!
//! Los modelos de wire vienen del crate `etsi` (port 1:1 del Python
//! `ETSIQKD/`). Aquí sólo hacemos la traducción HTTP↔modelo y la
//! consulta al `LinkBuffer`.
//!
//! Endpoints:
//!
//! ```text
//!   GET  /healthz                                        liveness
//!   GET  /api/v1/keys/{sae_id}/status                    Etsi014Status
//!   GET  /api/v1/keys/{sae_id}/enc_keys?number=N&size=B  Etsi014KeyContainer
//!   GET  /api/v1/keys/{sae_id}/dec_keys?key_ID=<uuid>    Etsi014KeyContainer
//!   POST /api/v1/keys/{sae_id}/dec_keys                  Etsi014KeyContainer
//! ```
//!
//! `{sae_id}` se acepta pero no se usa para enrutar — este quditto
//! simula un único enlace y sirve todas las requests con el mismo
//! buffer. Va al campo `slave_SAE_ID` del `Status` para que la
//! response sea ETSI-compliant.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use etsi::{
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
        // Quditto simula un enlace, no varios KMEs distintos. Marcamos
        // ambos KME_IDs como "quditto" y dejamos slave_SAE_ID = lo que
        // pidió el caller.
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
    /// La spec del proyecto fija 256 bits. Aceptamos el parámetro por
    /// compatibilidad ETSI 014 pero devolvemos 400 si no coincide.
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

    let keys: Vec<Etsi014Key> = taken
        .into_iter()
        .map(|k| Etsi014Key {
            key_id: k.key_id,
            key_id_extension: None,
            key: Base64Bytes::new(k.material),
            key_extension: None,
        })
        .collect();

    Json(Etsi014KeyContainer { keys, key_container_extension: None }).into_response()
}

#[derive(Deserialize, Debug)]
struct DecQuery {
    /// UUID de la clave que se quiere recuperar.
    #[serde(rename = "key_ID")]
    key_id: Uuid,
}

async fn get_dec_keys(
    State(svc): State<QudittoService>,
    Path(_sae_id): Path<String>,
    Query(q): Query<DecQuery>,
) -> Response {
    match svc.link.take_for_dec(&q.key_id) {
        Some(material) => Json(Etsi014KeyContainer {
            keys: vec![Etsi014Key {
                key_id: q.key_id,
                key_id_extension: None,
                key: Base64Bytes::new(material),
                key_extension: None,
            }],
            key_container_extension: None,
        })
        .into_response(),
        None => etsi_error(
            StatusCode::NOT_FOUND,
            format!("key_id {} not found (already consumed or never issued)", q.key_id),
        ),
    }
}

async fn post_dec_keys(
    State(svc): State<QudittoService>,
    Path(_sae_id): Path<String>,
    Json(body): Json<Etsi014KeyIDs>,
) -> Response {
    if body.key_ids.is_empty() {
        return etsi_error(StatusCode::BAD_REQUEST, "key_IDs must be non-empty".into());
    }

    let mut found = Vec::with_capacity(body.key_ids.len());
    let mut missing: Vec<String> = Vec::new();
    for id in &body.key_ids {
        match svc.link.take_for_dec(&id.key_id) {
            Some(material) => found.push(Etsi014Key {
                key_id: id.key_id,
                key_id_extension: None,
                key: Base64Bytes::new(material),
                key_extension: None,
            }),
            None => missing.push(id.key_id.to_string()),
        }
    }

    if !missing.is_empty() {
        // ETSI 014 §6.2: si alguno falta, todo el batch falla (404).
        // Devolvemos los IDs que faltaban en `details`.
        return etsi_error_with_details(
            StatusCode::NOT_FOUND,
            "one or more key_IDs not found".into(),
            missing,
        );
    }

    Json(Etsi014KeyContainer { keys: found, key_container_extension: None }).into_response()
}

// ─────────────────────────── helpers ─────────────────────────────

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
