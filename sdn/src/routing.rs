//! Path computation over the QKC graph.
//!
//! Single-path lookup for `ComputePath`: BFS by hops, matching the Python
//! SDN's `Topology.shortest_path_qkc`. The production forwarding tables are
//! NOT built here — they come from `mcmcf::wcmp_from_topology` (multipath,
//! capacity-weighted). `Policy` is accepted on the wire for compatibility,
//! but every variant resolves to shortest-hops; the first request with any
//! other policy says so once in the log.

use crate::{
    error::{Result, SdnError},
    topology::TopologyStore,
};

#[derive(Debug, Clone, Copy)]
pub enum Policy {
    ShortestHops,
    MinLatency,
    MaxAvailableCapacity,
    MinCostFlow,
}

#[derive(Debug, Clone)]
pub struct Path {
    /// QKC ids from `src` to `dst`, inclusive.
    pub nodes: Vec<String>,
    pub estimated_latency_us: u64,
    /// Smallest Quditto capacity (keys/s) along the path, rounded to bps-ish
    /// for the legacy gRPC field.
    pub bottleneck_capacity_bps: u64,
}

/// Propagación en fibra, ~5 µs/km. Es una estimación honesta a partir de la
/// `distance_km` declarada, no una medida.
const FIBRE_US_PER_KM: u64 = 5;

pub fn compute(
    store: &TopologyStore,
    src: &str,
    dst: &str,
    _required_bps: u64,
    policy: Policy,
) -> Result<Path> {
    if !matches!(policy, Policy::ShortestHops) {
        static SAID: std::sync::Once = std::sync::Once::new();
        SAID.call_once(|| {
            tracing::warn!(
                ?policy,
                "routing: la política pedida no está implementada; ComputePath resuelve por \
                 saltos (las tablas de producción salen de wcmp_from_topology)"
            );
        });
    }
    let topo = store.load();
    if !topo.qkcs.contains_key(src) && !topo.graph.contains_key(src) {
        return Err(SdnError::UnknownNode(src.into()));
    }
    if !topo.qkcs.contains_key(dst) && !topo.graph.contains_key(dst) {
        return Err(SdnError::UnknownNode(dst.into()));
    }
    let nodes = topo
        .shortest_path_qkc(src, dst)
        .ok_or_else(|| SdnError::NoPath(src.into(), dst.into()))?;

    let mut bottleneck = f64::INFINITY;
    let mut latency_us: u64 = 0;
    for w in nodes.windows(2) {
        if let Some(meta) = topo.edge(&w[0], &w[1]) {
            latency_us += u64::from(meta.distance_km) * FIBRE_US_PER_KM;
            // PQC hops are uncapacitated; they must not cap the bottleneck.
            if meta.is_pqc() {
                continue;
            }
            // Punto único de capacidad: tasa medida si la hay, fórmula si no.
            bottleneck = bottleneck.min(meta.capacity_keys_per_second());
        }
    }
    let bottleneck_bps = if bottleneck.is_finite() {
        bottleneck as u64
    } else {
        0
    };

    Ok(Path {
        nodes,
        estimated_latency_us: latency_us,
        bottleneck_capacity_bps: bottleneck_bps,
    })
}
