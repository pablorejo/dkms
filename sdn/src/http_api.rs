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
//!
//! Auth/JWT is intentionally out of scope here; the web frontend gates
//! access at its own layer.

use std::str::FromStr;

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
    error::SdnError,
    routing,
    service::SdnService,
    topology::{Dkms, Sae, Topology},
};

#[derive(Serialize)]
struct Health { status: &'static str }

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
    Json(svc.topology.load().qkcs.values().cloned().collect::<Vec<_>>())
}

async fn get_orrs(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(svc.topology.load().orrs.values().cloned().collect::<Vec<_>>())
}

async fn get_dkms_all(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(svc.topology.load().dkms.values().cloned().collect::<Vec<_>>())
}

async fn get_saes(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(svc.topology.load().saes.values().cloned().collect::<Vec<_>>())
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
                crate::priority::BufferRole::EncKeys => entry.0 = *rate,
                crate::priority::BufferRole::DecKeys => entry.1 = *rate,
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

/// POST /priority — el DKMS reporta el class de uno o varios de sus
/// buffers. Equivalente al `PATCH /flows/{flow_id}/class` del Python.
///
/// Body:
/// ```json
/// {
///   "updates": [
///     {"dkms_id": "dkms-11", "peer": "dkms-22", "role": "enc_keys", "class": "important"},
///     {"dkms_id": "dkms-11", "peer": "dkms-33", "role": "enc_keys", "class": "best_effort"}
///   ]
/// }
/// ```
///
/// `role`: `enc_keys` | `dec_keys`.
/// `class`: `priority` | `important` | `quickly` | `relax` | `best_effort` | `saturated`.
///
/// Tras aplicar, dispara `recompute_mcf` para que `/rate` refleje las
/// rates nuevas inmediatamente (sin esperar al periódico de 5 s).
#[derive(Deserialize)]
struct PriorityUpdate {
    dkms_id: String,
    peer:    String,
    role:    String,
    class:   String,
}

#[derive(Deserialize)]
struct PriorityBatch {
    updates: Vec<PriorityUpdate>,
}

async fn post_priority(
    State(svc): State<SdnService>,
    Json(body): Json<PriorityBatch>,
) -> impl IntoResponse {
    let topology = svc.topology.load();
    let mut applied = 0;
    let mut errors: Vec<String> = Vec::new();
    for u in body.updates.iter() {
        if !topology.dkms.contains_key(&u.dkms_id) {
            errors.push(format!("unknown dkms {}", u.dkms_id));
            continue;
        }
        if !topology.dkms.contains_key(&u.peer) {
            errors.push(format!("unknown peer {}", u.peer));
            continue;
        }
        let role = match crate::priority::BufferRole::from_str(&u.role) {
            Ok(r) => r,
            Err(e) => { errors.push(format!("role: {e}")); continue; }
        };
        let pri = match crate::priority::TrafficPriority::from_str(&u.class) {
            Ok(p) => p,
            Err(e) => { errors.push(format!("class: {e}")); continue; }
        };
        svc.priorities.set(&u.dkms_id, &u.peer, role, pri);
        applied += 1;
    }
    if applied > 0 {
        // Recompute inmediato para que `GET /rate` vea las nuevas rates.
        // No es caro: el MCF tarda <10ms para 12 commodities.
        let _ = svc.recompute_mcf();
    }
    let status = if errors.is_empty() { StatusCode::OK } else { StatusCode::PARTIAL_CONTENT };
    (status, Json(json!({"applied": applied, "errors": errors})))
}

async fn get_priorities(State(svc): State<SdnService>) -> impl IntoResponse {
    let snap = svc.priorities.snapshot();
    let arr: Vec<Value> = snap
        .into_iter()
        .map(|((dkms, peer, role), pri)| {
            json!({
                "dkms_id": dkms,
                "peer":    peer,
                "role":    role.as_str(),
                "class":   pri.as_str(),
            })
        })
        .collect();
    Json(json!({"priorities": arr}))
}

async fn get_links(State(svc): State<SdnService>) -> impl IntoResponse {
    let t = svc.topology.load();
    let v: Vec<_> = t.edges.iter().map(|((a, b), meta)| {
        json!({
            "a": a,
            "b": b,
            "distance_km":        meta.distance_km,
            "r0_keys_per_second": meta.r0_keys_per_second,
            "alpha":              meta.alpha,
            "max_buffer_size":    meta.max_buffer_size,
            "capacity_keys_per_second": meta.quditto_capacity_keys_per_second(),
        })
    }).collect();
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
        Ok(v)  => Json(v).into_response(),
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
    let bindings: serde_json::Map<String, Value> = t.saes.iter().filter_map(|(sid, s)| {
        let d = t.dkms.get(&s.dkms_id)?;
        Some((sid.clone(), build_binding(&t, sid, d)))
    }).collect();
    Json(json!({
        "dkms_id":  dkms.id,
        "bindings": bindings,
    })).into_response()
}

// ---------------- SAE CRUD ---------------------------------------------------

#[derive(Deserialize)]
struct DkmsTargetPayload { ip: String, port: u16 }

#[derive(Deserialize)]
struct SaeCreatePayload {
    id: String,
    #[serde(default)] dkms_id: Option<String>,
    #[serde(default)] dkms_target: Option<DkmsTargetPayload>,
}

#[derive(Deserialize)]
struct SaeUpdatePayload {
    #[serde(default)] dkms_id: Option<String>,
    #[serde(default)] dkms_target: Option<DkmsTargetPayload>,
}

async fn register_sae(
    State(svc): State<SdnService>,
    Json(p): Json<SaeCreatePayload>,
) -> impl IntoResponse {
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc.topology.register_sae(&p.id, p.dkms_id.as_deref(), target) {
        Ok(sae) => (StatusCode::CREATED, Json(sae)).into_response(),
        Err(e)  => err_response(e),
    }
}

async fn update_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
    Json(p): Json<SaeUpdatePayload>,
) -> impl IntoResponse {
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc.topology.update_sae(&sae_id, p.dkms_id.as_deref(), target) {
        Ok(sae) => Json(sae).into_response(),
        Err(e)  => err_response(e),
    }
}

async fn delete_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
) -> impl IntoResponse {
    match svc.topology.delete_sae(&sae_id) {
        Ok(())  => StatusCode::NO_CONTENT.into_response(),
        Err(e)  => err_response(e),
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
        return err_response(SdnError::BadRequest(
            "qkc_a and qkc_b are required".into(),
        ));
    }
    match svc.topology.update_edge_capacity_kps(&p.qkc_a, &p.qkc_b, p.capacity_keys_per_second) {
        Ok(changed) => Json(json!({
            "qkc_a": p.qkc_a,
            "qkc_b": p.qkc_b,
            "capacity_keys_per_second": p.capacity_keys_per_second,
            "changed": changed,
        })).into_response(),
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
fn default_policy() -> String { "shortest_hops".into() }

async fn compute_path(
    State(svc): State<SdnService>,
    Json(req): Json<PathRequest>,
) -> impl IntoResponse {
    let policy = match req.policy.as_str() {
        "min_latency"            => routing::Policy::MinLatency,
        "max_available_capacity" => routing::Policy::MaxAvailableCapacity,
        "min_cost_flow"          => routing::Policy::MinCostFlow,
        _                        => routing::Policy::ShortestHops,
    };
    match routing::compute(&svc.topology, &req.src, &req.dst, req.required_bps, policy) {
        Ok(p) => Json(json!({
            "path": p.nodes,
            "estimated_latency_us": p.estimated_latency_us,
            "bottleneck_capacity_bps": p.bottleneck_capacity_bps,
        })).into_response(),
        Err(e) => err_response(e),
    }
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
        UnknownNode(_)
        | UnknownLink(_)
        | UnknownDkms(_)
        | UnknownSae(_)
        | NoPath(..)             => StatusCode::NOT_FOUND,
        SaeAlreadyRegistered(_)  => StatusCode::CONFLICT,
        BadRequest(_)
        | Topology(_)
        | AdmissionDenied(_)     => StatusCode::BAD_REQUEST,
        _                        => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, msg)
}

// ---------------- server -----------------------------------------------------

pub async fn serve(svc: SdnService, addr: &str) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/healthz",                  get(healthz))
        .route("/topology",                 get(get_topology))
        .route("/qkcs",                     get(get_qkcs))
        .route("/orrs",                     get(get_orrs))
        .route("/dkms",                     get(get_dkms_all))
        .route("/saes",                     get(get_saes))
        .route("/links",                    get(get_links))
        .route("/sae",                      post(register_sae))
        .route("/sae/:sae_id",              put(update_sae).delete(delete_sae))
        .route("/sae/:sae_id/binding",      get(get_sae_binding))
        .route("/sae-bindings/:dkms_id",    get(list_sae_bindings))
        .route("/link-capacity",            post(update_link_capacity))
        .route("/paths",                    post(compute_path))
        .route("/rate/:dkms_id",            get(get_rate))
        .route("/priority",                 post(post_priority).get(get_priorities))
        .with_state(svc);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "sdn HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}
