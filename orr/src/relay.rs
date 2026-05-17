//! Circuit table + onion peeling.
//!
//! A `Circuit` is a sequence of NodeIds and a per-hop session key. When a
//! frame arrives, the next hop is looked up here, the outermost onion
//! layer is decrypted with this node's session key for that circuit, and
//! the resulting payload is forwarded to the next hop's ORR.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    Opening,
    Open,
    Closing,
    Closed,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Circuit {
    pub id: String,
    pub path: Vec<String>,               // ordered NodeIds
    pub session_keys: Arc<Vec<Vec<u8>>>, // per-hop, this node's view
    pub state: CircuitState,
    pub opened_at: DateTime<Utc>,
    pub last_used: DateTime<Utc>,
    pub frames: u64,
}

pub struct CircuitTable {
    circuits: DashMap<String, Circuit>,
}

impl CircuitTable {
    pub fn new() -> Self {
        Self {
            circuits: DashMap::new(),
        }
    }

    pub fn insert(&self, c: Circuit) {
        self.circuits.insert(c.id.clone(), c);
    }

    pub fn get(&self, id: &str) -> Option<Circuit> {
        self.circuits.get(id).map(|r| r.clone())
    }

    pub fn remove(&self, id: &str) {
        self.circuits.remove(id);
    }

    pub fn touch(&self, id: &str) {
        if let Some(mut c) = self.circuits.get_mut(id) {
            c.last_used = Utc::now();
            c.frames += 1;
        }
    }

    pub fn len(&self) -> usize {
        self.circuits.len()
    }

    pub fn is_empty(&self) -> bool {
        self.circuits.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = Circuit> + '_ {
        self.circuits.iter().map(|r| r.clone())
    }
}

impl Default for CircuitTable {
    fn default() -> Self {
        Self::new()
    }
}
