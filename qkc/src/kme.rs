//! KME = Key Management Entity.
//!
//! Holds per-peer key buffers and the reservation table. Pulls fresh key
//! material from the local quditto via gRPC and feeds the binary TCP
//! transport on the hot path.
//!
//! State layout:
//!     peer NodeId  →  VecDeque<Key>   (FIFO consumption)
//!     reservation_id  →  ReservedKeys
//!
//! Concurrency: backed by `DashMap` so individual peer buffers can be
//! accessed without contending across peers.

use std::{collections::VecDeque, sync::Arc};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use parking_lot::Mutex;

use crate::{config::QkcConfig, error::Result};

#[derive(Debug, Clone)]
pub struct Key {
    pub id:    String,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct ReservedKeys {
    pub reservation_id: String,
    pub peer:           String,
    pub keys:           Vec<Key>,
    pub expires_at:     DateTime<Utc>,
}

pub struct Kme {
    pub cfg:        Arc<QkcConfig>,
    pub buffers:    DashMap<String, Mutex<VecDeque<Key>>>, // peer -> buffer
    pub reservations: DashMap<String, ReservedKeys>,
}

impl Kme {
    pub async fn new(cfg: Arc<QkcConfig>) -> Result<Self> {
        Ok(Self {
            cfg,
            buffers: DashMap::new(),
            reservations: DashMap::new(),
        })
    }

    /// Pop `count` keys from the peer's buffer. Returns `None` if there
    /// aren't enough — caller decides whether to wait or fail.
    pub fn take(&self, peer: &str, count: usize) -> Option<Vec<Key>> {
        let entry = self.buffers.get(peer)?;
        let mut buf = entry.lock();
        if buf.len() < count {
            return None;
        }
        Some((0..count).filter_map(|_| buf.pop_front()).collect())
    }

    /// Push freshly minted keys into the peer's buffer (called by the
    /// quditto-puller background task).
    pub fn push(&self, peer: &str, keys: Vec<Key>) {
        let entry = self
            .buffers
            .entry(peer.to_owned())
            .or_insert_with(|| Mutex::new(VecDeque::new()));
        let mut buf = entry.lock();
        let cap = self.cfg.buffer_max_keys as usize;
        for k in keys {
            if buf.len() >= cap {
                buf.pop_front(); // drop oldest
            }
            buf.push_back(k);
        }
    }

    pub fn available(&self, peer: &str) -> u64 {
        self.buffers.get(peer).map_or(0, |b| b.lock().len() as u64)
    }
}
