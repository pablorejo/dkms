//! Path computation policies. Read-only over [`TopologyStore`].

use std::collections::BinaryHeap;

use crate::{
    error::{Result, SdnError},
    topology::TopologyStore,
};

pub enum Policy {
    ShortestHops,
    MinLatency,
    MaxAvailableCapacity,
    MinCostFlow,
}

#[derive(Debug, Clone)]
pub struct Path {
    pub nodes:                Vec<String>,
    pub estimated_latency_us: u64,
    pub bottleneck_capacity_bps: u64,
}

pub fn compute(store: &TopologyStore, src: &str, dst: &str, _required_bps: u64, policy: Policy) -> Result<Path> {
    let topo = store.load();
    let s = *topo.by_node.get(src).ok_or_else(|| SdnError::UnknownNode(src.into()))?;
    let t = *topo.by_node.get(dst).ok_or_else(|| SdnError::UnknownNode(dst.into()))?;

    // Dijkstra over the chosen weight metric. petgraph's `dijkstra` returns
    // distances, not parents — so we roll our own to capture the path.
    use petgraph::visit::EdgeRef;
    #[derive(PartialEq, Eq)]
    struct State { cost: u64, node: petgraph::stable_graph::NodeIndex }
    impl Ord for State {
        fn cmp(&self, o: &Self) -> std::cmp::Ordering { o.cost.cmp(&self.cost) }
    }
    impl PartialOrd for State {
        fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) }
    }

    let mut dist: std::collections::HashMap<_, u64> = std::collections::HashMap::new();
    let mut parent: std::collections::HashMap<_, petgraph::stable_graph::NodeIndex> = std::collections::HashMap::new();
    let mut heap = BinaryHeap::new();
    dist.insert(s, 0);
    heap.push(State { cost: 0, node: s });

    while let Some(State { cost, node }) = heap.pop() {
        if node == t { break; }
        if cost > *dist.get(&node).unwrap_or(&u64::MAX) { continue; }
        for er in topo.graph.edges(node) {
            let link = er.weight();
            if !link.up { continue; }
            let w = match policy {
                Policy::ShortestHops          => 1,
                Policy::MinLatency            => link.latency_us.max(1),
                Policy::MaxAvailableCapacity  => u64::MAX / (link.capacity_bps.max(1)),
                Policy::MinCostFlow           => link.latency_us.max(1), // placeholder
            };
            let new_cost = cost.saturating_add(w);
            let next = er.target();
            if new_cost < *dist.get(&next).unwrap_or(&u64::MAX) {
                dist.insert(next, new_cost);
                parent.insert(next, node);
                heap.push(State { cost: new_cost, node: next });
            }
        }
    }

    if !dist.contains_key(&t) {
        return Err(SdnError::NoPath(src.into(), dst.into()));
    }

    // Reconstruct path.
    let mut chain = vec![t];
    let mut cur = t;
    while cur != s {
        cur = *parent.get(&cur).ok_or_else(|| SdnError::NoPath(src.into(), dst.into()))?;
        chain.push(cur);
    }
    chain.reverse();

    let mut bottleneck = u64::MAX;
    let mut latency = 0u64;
    for w in chain.windows(2) {
        if let Some(e) = topo.graph.find_edge(w[0], w[1]) {
            if let Some(link) = topo.graph.edge_weight(e) {
                bottleneck = bottleneck.min(link.capacity_bps);
                latency = latency.saturating_add(link.latency_us);
            }
        }
    }

    Ok(Path {
        nodes: chain.into_iter().map(|i| topo.graph[i].id.clone()).collect(),
        estimated_latency_us: latency,
        bottleneck_capacity_bps: bottleneck,
    })
}
