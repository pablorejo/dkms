//! HTTP admin: lo que la SDN (o tú con `curl` durante pruebas) usa para
//! manipular la forwarding table.
//!
//! Endpoints:
//!
//! ```text
//!   GET  /healthz                                liveness
//!   GET  /forwarding-table                        snapshot actual
//!   POST /forwarding-table  body: { "replace": { "5": 2, "7": 2 } }
//!   POST /forwarding-table  body: { "updates": { "5": 2 }, "removes": [7] }
//! ```

use std::{collections::HashMap, sync::atomic::Ordering};

use axum::{extract::State, http::StatusCode, response::IntoResponse, routing::get, Json, Router};
use serde::Deserialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::service::QkcService;

pub fn router(svc: QkcService) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/forwarding-table", get(get_table).post(post_table))
        .route("/stats", get(get_stats))
        .with_state(svc)
}

pub async fn serve(svc: QkcService, addr: &str) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "qkc.http_admin.listening");
    axum::serve(listener, router(svc)).await?;
    Ok(())
}

async fn healthz(State(svc): State<QkcService>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "qkc_id": svc.qkc_id(),
        "links":  svc.cfg.links.len(),
    }))
}

async fn get_table(State(svc): State<QkcService>) -> impl IntoResponse {
    let snap = svc.routing.snapshot();
    Json(snap)
}

#[derive(Debug, Deserialize)]
struct TableBody {
    /// Si está presente, reemplaza la tabla completa.
    #[serde(default)]
    replace: Option<HashMap<String, u32>>,

    /// Delta: inserta/sobreescribe estas entradas.
    #[serde(default)]
    updates: Option<HashMap<String, u32>>,

    /// Delta: elimina estas entradas (claves como string).
    #[serde(default)]
    removes: Option<Vec<String>>,
}

async fn post_table(
    State(svc): State<QkcService>,
    Json(body): Json<TableBody>,
) -> impl IntoResponse {
    // Soporta dos modos: replace completo o delta.
    if let Some(replace) = body.replace {
        let parsed = match parse_map(&replace) {
            Ok(m) => m,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        svc.routing.replace(parsed);
        return Json(serde_json::json!({
            "mode": "replace",
            "size": svc.routing.snapshot().len(),
        }))
        .into_response();
    }

    let updates = body.updates.unwrap_or_default();
    let removes_raw = body.removes.unwrap_or_default();
    let updates_parsed = match parse_map(&updates) {
        Ok(m) => m,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut removes = Vec::with_capacity(removes_raw.len());
    for k in &removes_raw {
        match k.parse::<u32>() {
            Ok(v) => removes.push(v),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("remove key {k} is not u32"),
                )
                    .into_response();
            }
        }
    }
    svc.routing.update(updates_parsed, &removes);
    Json(serde_json::json!({
        "mode": "delta",
        "size": svc.routing.snapshot().len(),
    }))
    .into_response()
}

async fn get_stats(State(svc): State<QkcService>) -> impl IntoResponse {
    let s = &svc.stats;
    let service = serde_json::json!({
        "deliver_calls": s.deliver_calls.load(Ordering::Relaxed),
        "deliver_sends_ok": s.deliver_sends_ok.load(Ordering::Relaxed),
        "deliver_drops_full": s.deliver_drops_full.load(Ordering::Relaxed),
        "deliver_drops_closed": s.deliver_drops_closed.load(Ordering::Relaxed),
        "local_send_starts": s.local_send_starts.load(Ordering::Relaxed),
        "local_send_oks": s.local_send_oks.load(Ordering::Relaxed),
        "local_send_errs": s.local_send_errs.load(Ordering::Relaxed),
        "incoming_starts": s.incoming_starts.load(Ordering::Relaxed),
        "incoming_delivered": s.incoming_delivered.load(Ordering::Relaxed),
        "incoming_forwarded": s.incoming_forwarded.load(Ordering::Relaxed),
        "incoming_errs": s.incoming_errs.load(Ordering::Relaxed),
        "local_out_registered": svc.local_out.lock().len(),
    });
    let mut links = serde_json::Map::new();
    for (peer_id, link) in svc.links.iter() {
        let l = link.keys.levels();
        let (sent, dropped) = svc.peer_out.stats(*peer_id).unwrap_or((0, 0));
        links.insert(
            peer_id.to_string(),
            serde_json::json!({
                "enc_buffered": l.enc_buffered,
                "dec_buffered": l.dec_buffered,
                "enc_taken": l.enc_taken,
                "dec_lookups": l.dec_lookups,
                "dec_misses": l.dec_misses,
                "refills_enc": l.refills_enc,
                "refills_dec": l.refills_dec,
                "wait_enc_called": l.wait_enc_called,
                "wait_enc_succeeded": l.wait_enc_succeeded,
                "wait_enc_timeouts": l.wait_enc_timeouts,
                "wait_dec_called": l.wait_dec_called,
                "wait_dec_succeeded": l.wait_dec_succeeded,
                "wait_dec_timeouts": l.wait_dec_timeouts,
                "peer_out_sent": sent,
                "peer_out_dropped": dropped,
            }),
        );
    }
    Json(serde_json::json!({
        "qkc_id": svc.qkc_id(),
        "service": service,
        "links": links,
    }))
}

fn parse_map(raw: &HashMap<String, u32>) -> Result<HashMap<u32, u32>, String> {
    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        let kk: u32 = k.parse().map_err(|_| format!("key {k} is not u32"))?;
        out.insert(kk, *v);
    }
    Ok(out)
}
