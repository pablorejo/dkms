//! Path computation over the QKC graph.
//!
//! The richer routing policies (min-latency, max-available-capacity,
//! min-cost-flow) will land alongside the MCF solver in a follow-up; for
//! now this module exposes a BFS-based shortest-path lookup that matches
//! the Python SDN's [`Topology.shortest_path_qkc`] semantics.

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

pub fn compute(
    store: &TopologyStore,
    src: &str,
    dst: &str,
    _required_bps: u64,
    _policy: Policy,
) -> Result<Path> {
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
    for w in nodes.windows(2) {
        if let Some(meta) = topo.edge(&w[0], &w[1]) {
            bottleneck = bottleneck.min(meta.quditto_capacity_keys_per_second());
        }
    }
    let bottleneck_bps = if bottleneck.is_finite() { bottleneck as u64 } else { 0 };

    Ok(Path {
        nodes,
        estimated_latency_us: 0,
        bottleneck_capacity_bps: bottleneck_bps,
    })
}
