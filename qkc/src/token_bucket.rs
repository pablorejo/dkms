//! Per-link token-bucket admission control.
//!
//! Buckets are keyed by (src_node, dst_node) — i.e. a directed link. Tokens
//! refill at `refill_rate_keys_per_sec` and the bucket holds at most
//! `capacity_keys`. Each `try_consume(n)` checks the current count first.
//!
//! This mirrors the Python `QKC/TockenBucket.py` but uses a lock-free
//! per-bucket atomic clock and DashMap for the registry so multiple links
//! don't share a lock.

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use dashmap::DashMap;
use parking_lot::Mutex;

pub struct TokenBucket {
    pub capacity:    u64,
    pub refill_rate: u64, // keys/s
    state: Mutex<BucketState>,
}

struct BucketState {
    tokens:     u64,
    last_refill: Instant,
}

impl TokenBucket {
    pub fn new(capacity: u64, refill_rate: u64) -> Self {
        Self {
            capacity,
            refill_rate,
            state: Mutex::new(BucketState {
                tokens: capacity,
                last_refill: Instant::now(),
            }),
        }
    }

    pub fn try_consume(&self, n: u64) -> bool {
        let mut s = self.state.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(s.last_refill).as_secs_f64();
        let refilled = (elapsed * self.refill_rate as f64) as u64;
        s.tokens = (s.tokens + refilled).min(self.capacity);
        s.last_refill = now;

        if s.tokens >= n {
            s.tokens -= n;
            true
        } else {
            false
        }
    }

    pub fn available(&self) -> u64 {
        self.state.lock().tokens
    }
}

pub struct TokenBucketRegistry {
    pub default_rate: AtomicU64,
    buckets: DashMap<String, TokenBucket>,
}

impl TokenBucketRegistry {
    pub fn new(default_rate: u64) -> Self {
        Self {
            default_rate: AtomicU64::new(default_rate),
            buckets: DashMap::new(),
        }
    }

    pub fn get_or_create(&self, link_id: &str, capacity: u64) -> dashmap::mapref::one::Ref<'_, String, TokenBucket> {
        if !self.buckets.contains_key(link_id) {
            let rate = self.default_rate.load(Ordering::Relaxed);
            self.buckets.insert(link_id.to_owned(), TokenBucket::new(capacity, rate));
        }
        self.buckets.get(link_id).expect("just inserted")
    }

    pub fn update(&self, link_id: &str, capacity: u64, refill_rate: u64) {
        self.buckets.insert(link_id.to_owned(), TokenBucket::new(capacity, refill_rate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_overspend() {
        let b = TokenBucket::new(10, 100);
        assert!(b.try_consume(5));
        assert!(b.try_consume(5));
        assert!(!b.try_consume(1)); // 0 left immediately after
    }
}
