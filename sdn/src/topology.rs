//! In-memory topology graph.
//!
//! Built on `petgraph::stable_graph::StableDiGraph` so node/edge indices
//! stay stable across mutations (we hand them out to other modules in
//! routing responses, so we can't have them shift around). Wrapped in an
//! `ArcSwap<Topology>` so reads are lock-free.

use std::collections::HashMap;

use arc_swap::ArcSwap;
use petgraph::stable_graph::{NodeIndex, StableDiGraph};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id:       String,
    pub hostname: String,
    #[serde(default)]
    pub labels:   HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id:          String,
    pub src:         String,
    pub dst:         String,
    pub capacity_bps: u64,
    pub keys_per_sec: u64,
    pub latency_us:  u64,
    pub up:          bool,
    #[serde(default)]
    pub attributes:  HashMap<String, String>,
}

#[derive(Debug, Default)]
pub struct Topology {
    pub graph:   StableDiGraph<Node, Link>,
    pub by_node: HashMap<String, NodeIndex>,
    pub by_link: HashMap<String, (NodeIndex, NodeIndex)>,
    pub version: i64,
}

impl Topology {
    pub fn add_node(&mut self, n: Node) -> NodeIndex {
        let id = n.id.clone();
        let idx = self.graph.add_node(n);
        self.by_node.insert(id, idx);
        idx
    }

    pub fn add_link(&mut self, l: Link) -> Option<()> {
        let s = *self.by_node.get(&l.src)?;
        let d = *self.by_node.get(&l.dst)?;
        let id = l.id.clone();
        self.graph.add_edge(s, d, l);
        self.by_link.insert(id, (s, d));
        Some(())
    }

    pub fn from_json(path: &str) -> std::io::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        let parsed: serde_json::Value = serde_json::from_str(&raw)?;
        let mut t = Topology::default();
        if let Some(nodes) = parsed.get("nodes").and_then(|v| v.as_array()) {
            for n in nodes {
                if let Ok(node) = serde_json::from_value::<Node>(n.clone()) {
                    t.add_node(node);
                }
            }
        }
        if let Some(links) = parsed.get("links").and_then(|v| v.as_array()) {
            for l in links {
                if let Ok(link) = serde_json::from_value::<Link>(l.clone()) {
                    t.add_link(link);
                }
            }
        }
        Ok(t)
    }
}

/// Cheap-to-clone wrapper. Each mutation builds a new `Topology` and swaps
/// the pointer — readers never block.
#[derive(Clone, Default)]
pub struct TopologyStore {
    inner: std::sync::Arc<ArcSwap<Topology>>,
}

impl TopologyStore {
    pub fn new(initial: Topology) -> Self {
        Self {
            inner: std::sync::Arc::new(ArcSwap::from_pointee(initial)),
        }
    }

    pub fn load(&self) -> std::sync::Arc<Topology> {
        self.inner.load_full()
    }

    pub fn replace(&self, new: Topology) {
        self.inner.store(std::sync::Arc::new(new));
    }
}
