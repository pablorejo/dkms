//! Maintains gRPC clients to peer ORRs. New peer endpoints can be pushed
//! at runtime by SDN; we don't connect eagerly — that's done on first use.

use std::collections::HashMap;

use parking_lot::RwLock;

pub struct PeerRegistry {
    by_node: RwLock<HashMap<String, String>>, // NodeId -> gRPC URL
}

impl PeerRegistry {
    pub fn new(seed: HashMap<String, String>) -> Self {
        Self {
            by_node: RwLock::new(seed),
        }
    }

    pub fn put(&self, node: String, url: String) {
        self.by_node.write().insert(node, url);
    }

    pub fn get(&self, node: &str) -> Option<String> {
        self.by_node.read().get(node).cloned()
    }

    pub fn remove(&self, node: &str) {
        self.by_node.write().remove(node);
    }
}
