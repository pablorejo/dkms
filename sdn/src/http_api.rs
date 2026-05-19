//! HTTP admin API consumed by the web UI and the orchestrator.
//!
//! Read-only:
//!   GET    /healthz
//!   GET    /topology
//!   GET    /qkcs /orrs /dkms /saes /links
//!   GET    /sae/{sae_id}/binding
//!   GET    /sae-bindings/{dkms_id}
//!
//! Mutations (mirror Python SDN semantics):
//!   POST   /sae                       register a SAE
//!   PUT    /sae/{sae_id}              re-bind a SAE to another DKMS
//!   DELETE /sae/{sae_id}              remove a SAE
//!   POST   /link-capacity             notify a link-capacity change
//!   POST   /paths                     compute a path
//!   POST   /demand                    DKMS reports (L_k, B_k, δ_k) per commodity
//!   GET    /demand                    inspect the demand registry
//!
//! Auth/JWT is intentionally out of scope here; the web frontend gates
//! access at its own layer.

use axum::{
    extract::{Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::info;

use crate::{
    demand::{CommodityDemand, DemandReport},
    error::SdnError,
    routing,
    service::SdnService,
    topology::{Dkms, Sae, Topology},
};

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

// ---------------- read-only --------------------------------------------------

async fn get_topology(State(svc): State<SdnService>) -> impl IntoResponse {
    let t = svc.topology.load();
    Json(json!({
        "version": t.version,
        "qkcs":    t.qkcs.len(),
        "orrs":    t.orrs.len(),
        "dkms":    t.dkms.len(),
        "saes":    t.saes.len(),
        "edges":   t.edges.len(),
    }))
}

async fn get_qkcs(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .qkcs
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_orrs(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .orrs
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_dkms_all(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .dkms
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_saes(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .saes
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

/// GET /rate/{dkms_id} — devuelve las rates per-peer asignadas por el MCF solver.
///
/// Formato:
/// ```json
/// {
///   "dkms_id": "dkms-11",
///   "version": 7,
///   "peers": {
///     "dkms-22": {"enc": 333.0, "dec": 333.0},
///     "dkms-33": {"enc": 333.0, "dec": 333.0}
///   }
/// }
/// ```
///
/// El DKMS hace polling cada N segundos (típico 5s, configurable) y usa la
/// rate como refill del token bucket per-peer del Generator.
async fn get_rate(
    AxumPath(dkms_id): AxumPath<String>,
    State(svc): State<SdnService>,
) -> impl IntoResponse {
    let snap = svc.mcf_snapshot.load();
    let topology = svc.topology.load();
    if !topology.dkms.contains_key(&dkms_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("dkms {dkms_id} unknown")})),
        );
    }
    let mut peers = serde_json::Map::new();
    if let Some(my_rates) = snap.rates_by_dkms.get(&dkms_id) {
        let mut grouped: std::collections::HashMap<String, (f64, f64)> =
            std::collections::HashMap::new();
        for ((peer, role), rate) in my_rates.iter() {
            let entry = grouped.entry(peer.clone()).or_insert((0.0, 0.0));
            match role {
                crate::mcf::BufferRole::EncKeys => entry.0 = *rate,
                crate::mcf::BufferRole::DecKeys => entry.1 = *rate,
            }
        }
        for (peer, (enc, dec)) in grouped {
            peers.insert(peer, json!({"enc": enc, "dec": dec}));
        }
    }
    let topo_version = topology.version;
    (
        StatusCode::OK,
        Json(json!({
            "dkms_id":          dkms_id,
            "topology_version": topo_version,
            "peers":            peers,
        })),
    )
}

async fn get_links(State(svc): State<SdnService>) -> impl IntoResponse {
    let t = svc.topology.load();
    let v: Vec<_> = t
        .edges
        .iter()
        .map(|((a, b), meta)| {
            json!({
                "a": a,
                "b": b,
                "distance_km":        meta.distance_km,
                "r0_keys_per_second": meta.r0_keys_per_second,
                "alpha":              meta.alpha,
                "max_buffer_size":    meta.max_buffer_size,
                "capacity_keys_per_second": meta.quditto_capacity_keys_per_second(),
            })
        })
        .collect();
    Json(v)
}

fn binding_json(topo: &Topology, sae: &Sae) -> Result<Value, SdnError> {
    let dkms = topo
        .dkms
        .get(&sae.dkms_id)
        .ok_or_else(|| SdnError::UnknownDkms(sae.dkms_id.clone()))?;
    Ok(build_binding(topo, &sae.id, dkms))
}

fn build_binding(topo: &Topology, sae_id: &str, dkms: &Dkms) -> Value {
    let orr_id = topo.orrs.get(&dkms.orr_id).map(|o| o.id.clone());
    json!({
        "sae_id": sae_id,
        "dkms_id": dkms.id,
        "dkms_endpoint": { "id": dkms.host.id, "ip": dkms.host.ip, "port": dkms.host.port },
        "orr_id": orr_id,
    })
}

async fn get_sae_binding(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
) -> impl IntoResponse {
    let t = svc.topology.load();
    let Some(sae) = t.saes.get(&sae_id) else {
        return err_response(SdnError::UnknownSae(sae_id));
    };
    match binding_json(&t, sae) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn list_sae_bindings(
    State(svc): State<SdnService>,
    AxumPath(dkms_id): AxumPath<String>,
) -> impl IntoResponse {
    let t = svc.topology.load();
    let Some(dkms) = t.dkms.get(&dkms_id) else {
        return err_response(SdnError::UnknownDkms(dkms_id));
    };
    // Python returns the FULL table (not filtered by dkms_id). DKMS uses it
    // as a single bootstrap call to warm its SaeBindingCache.
    let bindings: serde_json::Map<String, Value> = t
        .saes
        .iter()
        .filter_map(|(sid, s)| {
            let d = t.dkms.get(&s.dkms_id)?;
            Some((sid.clone(), build_binding(&t, sid, d)))
        })
        .collect();
    Json(json!({
        "dkms_id":  dkms.id,
        "bindings": bindings,
    }))
    .into_response()
}

// ---------------- SAE CRUD ---------------------------------------------------

#[derive(Deserialize)]
struct DkmsTargetPayload {
    ip: String,
    port: u16,
}

#[derive(Deserialize)]
struct SaeCreatePayload {
    id: String,
    #[serde(default)]
    dkms_id: Option<String>,
    #[serde(default)]
    dkms_target: Option<DkmsTargetPayload>,
}

#[derive(Deserialize)]
struct SaeUpdatePayload {
    #[serde(default)]
    dkms_id: Option<String>,
    #[serde(default)]
    dkms_target: Option<DkmsTargetPayload>,
}

async fn register_sae(
    State(svc): State<SdnService>,
    Json(p): Json<SaeCreatePayload>,
) -> impl IntoResponse {
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc
        .topology
        .register_sae(&p.id, p.dkms_id.as_deref(), target)
    {
        Ok(sae) => (StatusCode::CREATED, Json(sae)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn update_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
    Json(p): Json<SaeUpdatePayload>,
) -> impl IntoResponse {
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc
        .topology
        .update_sae(&sae_id, p.dkms_id.as_deref(), target)
    {
        Ok(sae) => Json(sae).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
) -> impl IntoResponse {
    match svc.topology.delete_sae(&sae_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- link capacity (paper §4.3 trigger 2) -----------------------

#[derive(Deserialize)]
struct LinkCapacityPayload {
    qkc_a: String,
    qkc_b: String,
    capacity_keys_per_second: f64,
}

async fn update_link_capacity(
    State(svc): State<SdnService>,
    Json(p): Json<LinkCapacityPayload>,
) -> impl IntoResponse {
    if p.qkc_a.is_empty() || p.qkc_b.is_empty() {
        return err_response(SdnError::BadRequest("qkc_a and qkc_b are required".into()));
    }
    match svc
        .topology
        .update_edge_capacity_kps(&p.qkc_a, &p.qkc_b, p.capacity_keys_per_second)
    {
        Ok(changed) => Json(json!({
            "qkc_a": p.qkc_a,
            "qkc_b": p.qkc_b,
            "capacity_keys_per_second": p.capacity_keys_per_second,
            "changed": changed,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- path compute -----------------------------------------------

#[derive(Deserialize)]
struct PathRequest {
    src: String,
    dst: String,
    #[serde(default)]
    required_bps: u64,
    #[serde(default = "default_policy")]
    policy: String,
}
fn default_policy() -> String {
    "shortest_hops".into()
}

async fn compute_path(
    State(svc): State<SdnService>,
    Json(req): Json<PathRequest>,
) -> impl IntoResponse {
    let policy = match req.policy.as_str() {
        "min_latency" => routing::Policy::MinLatency,
        "max_available_capacity" => routing::Policy::MaxAvailableCapacity,
        "min_cost_flow" => routing::Policy::MinCostFlow,
        _ => routing::Policy::ShortestHops,
    };
    match routing::compute(&svc.topology, &req.src, &req.dst, req.required_bps, policy) {
        Ok(p) => Json(json!({
            "path": p.nodes,
            "estimated_latency_us": p.estimated_latency_us,
            "bottleneck_capacity_bps": p.bottleneck_capacity_bps,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- demand (MCMCF-λ) -------------------------------------------

/// POST /demand — a DKMS reports the current `(L_k, B_k, δ_k)` for
/// every commodity it sources. Replaces the previous report for
/// matching `(src_dkms, dst_dkms)` keys.
///
/// Body:
/// ```json
/// {
///   "dkms_id": "dkms-11",
///   "entries": [
///     {
///       "src_dkms": "dkms-11", "dst_dkms": "dkms-22",
///       "level": 1234.0, "capacity": 65536.0,
///       "drain_rate": 50.0, "timestamp_ms": 1234567890
///     }
///   ]
/// }
/// ```
///
/// Returns `{accepted: N, errors: [...]}`. Status `200` if everything
/// landed, `206` (partial content) if some entries were rejected.
///
/// Phase 1 — storage only: the MCMCF-λ solver in phase 3 will pick
/// up these reports. The legacy MCF solver is unaffected.
async fn post_demand(
    State(svc): State<SdnService>,
    Json(report): Json<DemandReport>,
) -> impl IntoResponse {
    if report.dkms_id.is_empty() {
        return err_response(SdnError::BadRequest("dkms_id is required".into()));
    }
    let summary = svc.demand_registry.ingest(report);
    let status = if summary.errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    (
        status,
        Json(json!({
            "accepted": summary.accepted,
            "errors":   summary.errors,
            "registry_len": svc.demand_registry.len(),
        })),
    )
        .into_response()
}

/// GET /demand — dump every recorded `(src_dkms, dst_dkms)` demand
/// snapshot. Read-only inspection endpoint for dashboards / debugging.
async fn get_demand(State(svc): State<SdnService>) -> impl IntoResponse {
    let entries: Vec<CommodityDemand> = svc.demand_registry.snapshot();
    Json(json!({ "entries": entries, "len": entries.len() }))
}

/// GET /wcmp — dump the currently-published WCMP table per
/// (transit_qkc, dst_qkc) pair. Read-only inspection of what the LP
/// fed to the forwarding push loop. The shape mirrors
/// `McfSnapshot::wcmp`: `qkc → dst → [{qkc_id, weight}, ...]`.
async fn get_wcmp(State(svc): State<SdnService>) -> impl IntoResponse {
    let snap = svc.mcf_snapshot.load();
    Json(json!({
        "wcmp": &snap.wcmp,
        "n_qkcs": snap.wcmp.len(),
    }))
}

// ---------------- error mapping ----------------------------------------------

fn err_response(e: SdnError) -> axum::response::Response {
    let (code, msg) = http_status_for(&e);
    (code, Json(json!({ "detail": msg }))).into_response()
}

fn http_status_for(e: &SdnError) -> (StatusCode, String) {
    use SdnError::*;
    let msg = e.to_string();
    let code = match e {
        UnknownNode(_) | UnknownLink(_) | UnknownDkms(_) | UnknownSae(_) | NoPath(..) => {
            StatusCode::NOT_FOUND
        }
        SaeAlreadyRegistered(_) => StatusCode::CONFLICT,
        BadRequest(_) | Topology(_) | AdmissionDenied(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, msg)
}

// ---------------- server -----------------------------------------------------

pub async fn serve(svc: SdnService, addr: &str) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/topology", get(get_topology))
        .route("/qkcs", get(get_qkcs))
        .route("/orrs", get(get_orrs))
        .route("/dkms", get(get_dkms_all))
        .route("/saes", get(get_saes))
        .route("/links", get(get_links))
        .route("/sae", post(register_sae))
        .route("/sae/:sae_id", put(update_sae).delete(delete_sae))
        .route("/sae/:sae_id/binding", get(get_sae_binding))
        .route("/sae-bindings/:dkms_id", get(list_sae_bindings))
        .route("/link-capacity", post(update_link_capacity))
        .route("/paths", post(compute_path))
        .route("/rate/:dkms_id", get(get_rate))
        .route("/demand", post(post_demand).get(get_demand))
        .route("/wcmp", get(get_wcmp))
        .with_state(svc);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "sdn HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{demand::CommodityDemand, service::tests::make_service, service::SdnService};

    /// Bind a minimal SDN HTTP router (just the demand endpoints) to
    /// 127.0.0.1:0 and return the concrete `http://127.0.0.1:<port>`
    /// base URL. We don't reuse `serve()` directly because we want a
    /// stripped-down router that doesn't bind GRPC / metrics
    /// addresses too.
    async fn spawn_server(svc: SdnService) -> String {
        let app = Router::new()
            .route("/demand", post(post_demand).get(get_demand))
        .route("/wcmp", get(get_wcmp))
            .with_state(svc);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn post_demand_persists_entries_and_get_dumps_them() {
        let svc = make_service();
        let base = spawn_server(svc.clone()).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let body = serde_json::json!({
            "dkms_id": "dA",
            "entries": [
                {
                    "src_dkms": "dA", "dst_dkms": "dB",
                    "level": 100.0, "capacity": 4096.0,
                    "drain_rate": 30.0, "timestamp_ms": 1_000,
                },
                {
                    "src_dkms": "dA", "dst_dkms": "dC",
                    "level": 200.0, "capacity": 4096.0,
                    "drain_rate": 40.0, "timestamp_ms": 1_000,
                }
            ]
        });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["accepted"], 2);
        assert_eq!(v["registry_len"], 2);

        let g = client.get(format!("{base}/demand")).send().await.unwrap();
        assert_eq!(g.status(), 200);
        let dump: serde_json::Value = g.json().await.unwrap();
        assert_eq!(dump["len"], 2);
        let entries = dump["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);

        // Round-trip through serde — make sure CommodityDemand
        // deserialises cleanly from the GET output.
        let parsed: Vec<CommodityDemand> = serde_json::from_value(dump["entries"].clone()).unwrap();
        assert!(parsed.iter().any(|e| e.dst_dkms == "dB"));
        assert!(parsed.iter().any(|e| e.dst_dkms == "dC"));
    }

    #[tokio::test]
    async fn post_demand_returns_206_when_some_entries_rejected() {
        let svc = make_service();
        let base = spawn_server(svc).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let body = serde_json::json!({
            "dkms_id": "dA",
            "entries": [
                { "src_dkms": "dA", "dst_dkms": "dB",
                  "level": 100.0, "capacity": 4096.0,
                  "drain_rate": 30.0, "timestamp_ms": 1_000 },
                // src_dkms mismatch with report dkms_id → rejected
                { "src_dkms": "dB", "dst_dkms": "dC",
                  "level": 100.0, "capacity": 4096.0,
                  "drain_rate": 30.0, "timestamp_ms": 1_000 }
            ]
        });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["errors"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn post_demand_rejects_empty_dkms_id() {
        let svc = make_service();
        let base = spawn_server(svc).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let body = serde_json::json!({ "dkms_id": "", "entries": [] });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn post_demand_second_report_replaces_first() {
        let svc = make_service();
        let base = spawn_server(svc.clone()).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mk = |level: f64, ts: i64| {
            serde_json::json!({
                "dkms_id": "dA",
                "entries": [{
                    "src_dkms": "dA", "dst_dkms": "dB",
                    "level": level, "capacity": 4096.0,
                    "drain_rate": 30.0, "timestamp_ms": ts,
                }]
            })
        };
        client
            .post(format!("{base}/demand"))
            .json(&mk(100.0, 1000))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/demand"))
            .json(&mk(500.0, 2000))
            .send()
            .await
            .unwrap();
        let g = svc.demand_registry.get("dA", "dB").unwrap();
        assert_eq!(g.level, 500.0);
        assert_eq!(g.timestamp_ms, 2000);
        assert_eq!(svc.demand_registry.len(), 1);
    }
}
