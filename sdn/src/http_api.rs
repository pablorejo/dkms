//! HTTP admin API consumed by the web UI and the orchestrator.
//!
//! Read-only endpoints for now:
//!   GET  /healthz
//!   GET  /topology          full graph as JSON
//!   GET  /nodes
//!   GET  /links
//!   POST /paths             { src, dst, required_bps, policy } -> path
//!
//! Auth/JWT is intentionally out of scope here; the web frontend gates
//! access at its own layer.

use axum::{
    extract::State,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tracing::info;

use crate::{routing, service::SdnService};

#[derive(Serialize)]
struct Health { status: &'static str }

async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn get_topology(State(svc): State<SdnService>) -> impl IntoResponse {
    let topo = svc.topology.load();
    let nodes: Vec<_> = topo.graph.node_weights().cloned().collect();
    let links: Vec<_> = topo.graph.edge_weights().cloned().collect();
    Json(serde_json::json!({ "nodes": nodes, "links": links, "version": topo.version }))
}

async fn get_nodes(State(svc): State<SdnService>) -> impl IntoResponse {
    let topo = svc.topology.load();
    let nodes: Vec<_> = topo.graph.node_weights().cloned().collect();
    Json(nodes)
}

async fn get_links(State(svc): State<SdnService>) -> impl IntoResponse {
    let topo = svc.topology.load();
    let links: Vec<_> = topo.graph.edge_weights().cloned().collect();
    Json(links)
}

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
        "min_latency"           => routing::Policy::MinLatency,
        "max_available_capacity" => routing::Policy::MaxAvailableCapacity,
        "min_cost_flow"         => routing::Policy::MinCostFlow,
        _                        => routing::Policy::ShortestHops,
    };
    match routing::compute(&svc.topology, &req.src, &req.dst, req.required_bps, policy) {
        Ok(p) => Json(serde_json::json!({
            "path": p.nodes,
            "estimated_latency_us": p.estimated_latency_us,
            "bottleneck_capacity_bps": p.bottleneck_capacity_bps,
        })).into_response(),
        Err(e) => (axum::http::StatusCode::NOT_FOUND, e.to_string()).into_response(),
    }
}

pub async fn serve(svc: SdnService, addr: &str) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/healthz",  get(healthz))
        .route("/topology", get(get_topology))
        .route("/nodes",    get(get_nodes))
        .route("/links",    get(get_links))
        .route("/paths",    post(compute_path))
        .with_state(svc);

    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "sdn HTTP listening");
    axum::serve(listener, app).await?;
    Ok(())
}
