//! Per-(local_sae, remote_sae) key buffer.
//!
//! Keys arrive in batches from QKC and are buffered until the local SAE
//! reads them. The round-robin scheduler decides which (local, remote)
//! pair gets to consume next, subject to per-SAE token-bucket caps.

use std::collections::VecDeque;

use dashmap::DashMap;
use parking_lot::Mutex;

#[derive(Debug, Clone)]
pub struct BufferedKey {
    pub id:    String,
    pub bytes: Vec<u8>,
    pub size_bits: u32,
}

pub struct BufferRegistry {
    /// (local_sae, remote_sae) -> FIFO of keys
    buffers: DashMap<(String, String), Mutex<VecDeque<BufferedKey>>>,
}

impl BufferRegistry {
    pub fn new() -> Self {
        Self { buffers: DashMap::new() }
    }

    pub fn push(&self, local: &str, remote: &str, keys: Vec<BufferedKey>) {
        let entry = self
            .buffers
            .entry((local.into(), remote.into()))
            .or_insert_with(|| Mutex::new(VecDeque::new()));
        let mut q = entry.lock();
        for k in keys {
            q.push_back(k);
        }
    }

    pub fn take(&self, local: &str, remote: &str, count: usize) -> Vec<BufferedKey> {
        let Some(entry) = self.buffers.get(&(local.into(), remote.into())) else {
            return vec![];
        };
        let mut q = entry.lock();
        (0..count).filter_map(|_| q.pop_front()).collect()
    }

    pub fn len(&self, local: &str, remote: &str) -> usize {
        self.buffers
            .get(&(local.into(), remote.into()))
            .map_or(0, |e| e.lock().len())
    }
}

impl Default for BufferRegistry {
    fn default() -> Self { Self::new() }
}
