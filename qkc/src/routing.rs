//! Resolves "given final dst, which peer do I send the next hop to?".
//!
//! For the static-topology case this is a lookup against a routing table
//! pushed in by SDN. For the dynamic case the table is updated by streaming
//! `TopologyEvent`s from SDN.

use std::sync::Arc;

use dashmap::DashMap;

use crate::config::QkcConfig;

pub struct RoutingResolver {
    pub cfg: Arc<QkcConfig>,
    /// final_dst → next_hop NodeId
    table: DashMap<String, String>,
}

impl RoutingResolver {
    pub fn new(cfg: Arc<QkcConfig>) -> Self {
        Self {
            cfg,
            table: DashMap::new(),
        }
    }

    pub fn install(&self, dst: String, next_hop: String) {
        self.table.insert(dst, next_hop);
    }

    pub fn next_hop(&self, dst: &str) -> Option<String> {
        self.table.get(dst).map(|e| e.clone())
    }
}
