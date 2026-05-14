//! Round-robin scheduler.
//!
//! Walks the set of (local_sae, remote_sae) pairs with non-empty buffers
//! and grants delivery to the next eligible one each tick. "Eligible"
//! means: bucket has tokens, peer DKMS is reachable, ORR circuit is up.
//!
//! Internal pointer is a `RwLock<usize>` over a sorted Vec rebuilt on
//! demand. The scheduler does not hold buffers itself — it just decides
//! whose turn it is.

use parking_lot::RwLock;

pub struct Scheduler {
    pub period_ms: u64,
    cursor: RwLock<usize>,
}

impl Scheduler {
    pub fn new(period_ms: u64) -> Self {
        Self { period_ms, cursor: RwLock::new(0) }
    }

    pub fn next_index(&self, n: usize) -> Option<usize> {
        if n == 0 { return None; }
        let mut c = self.cursor.write();
        let i = *c % n;
        *c = (*c + 1) % n;
        Some(i)
    }
}
