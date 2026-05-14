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
        .with_state(svc);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "sdn HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}
