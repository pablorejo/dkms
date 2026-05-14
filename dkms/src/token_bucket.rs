//! Per-SAE token bucket. Identical algorithm to QKC's but keyed by SaeId
//! and (often) sized differently. Kept as a separate module so the SAE
//! rate-limit policy can evolve independently of the link-level one.

use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;

pub struct Bucket {
    pub capacity:    u64,
    pub refill_rate: u64,
    state: Mutex<State>,
}
struct State { tokens: u64, last: Instant }

impl Bucket {
    pub fn new(capacity: u64, refill_rate: u64) -> Self {
        Self { capacity, refill_rate, state: Mutex::new(State { tokens: capacity, last: Instant::now() }) }
    }

    pub fn try_consume(&self, n: u64) -> bool {
        let mut s = self.state.lock();
        let now = Instant::now();
        let elapsed = now.duration_since(s.last).as_secs_f64();
        let refilled = (elapsed * self.refill_rate as f64) as u64;
        s.tokens = (s.tokens + refilled).min(self.capacity);
        s.last = now;
        if s.tokens >= n { s.tokens -= n; true } else { false }
    }
}

pub struct SaeBuckets {
    default_rate: u64,
    map: DashMap<String, Bucket>,
}

impl SaeBuckets {
    pub fn new(default_rate: u64) -> Self {
        Self { default_rate, map: DashMap::new() }
    }

    pub fn ensure(&self, sae: &str, capacity: u64) {
        self.map.entry(sae.into()).or_insert_with(|| Bucket::new(capacity, self.default_rate));
    }

    pub fn try_take(&self, sae: &str, n: u64) -> bool {
        if let Some(b) = self.map.get(sae) {
            b.try_consume(n)
        } else {
            self.ensure(sae, self.default_rate * 4);
            self.map.get(sae).map_or(false, |b| b.try_consume(n))
        }
    }

    pub fn set_rate(&self, sae: &str, rate: u64, capacity: u64) {
        self.map.insert(sae.into(), Bucket::new(capacity, rate));
    }
}
