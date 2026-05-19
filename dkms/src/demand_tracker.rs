//! Per-peer SAE consumption tracker — feeds the SDN's MCMCF-λ solver
//! the `δ_k` (drain rate) it needs for each `(self, peer)` commodity.
//!
//! ## Where the samples come from
//!
//! Every ETSI 014 `enc_keys` request that targets a remote peer DKMS
//! consumes transport keys from `buffer_enc[peer]`. The exact number
//! is the per-peer `cost` computed in `service::handle_enc_keys`
//! (= `ceil(size / unit) × number` transport keys, with `num_dkms=1`
//! because we're attributing to a single peer at a time).
//!
//! We sample **before bucket admission**, per the design in the paper
//! (Sec. 4.2 sketch of the prerequisite for the SDN solver):
//!
//! > the counter increments **before** bucket admission (so demand
//! > reflects what the SAE asks, not what it receives).
//!
//! That choice matters: if you only count what the bucket admits,
//! starvation becomes self-reinforcing — a starved buffer reports
//! `δ_k ≈ 0` to the SDN, which then allocates it even less rate.
//! Counting demand instead lets the SDN see real pressure.
//!
//! ## EWMA model
//!
//! We bucket events into fixed `window_ms` slices (default 1 s) and
//! apply the smoothing
//!
//! ```text
//!     S(n+1) = (1 - α) · S(n) + α · sample(n)
//! ```
//!
//! where `sample(n)` is the keys-per-second observed in window `n`.
//! `α = 0.2` (the paper's suggested value) makes ~5 windows the
//! visible response time, which matches the SDN's `mcf_period_ms`
//! cadence (5 s default).
//!
//! Idle windows are processed lazily: `rate()` rolls forward through
//! the missed windows (each contributing a `sample = 0`), so a peer
//! that stops requesting will see its EWMA decay smoothly toward 0
//! without anyone having to call `record()`.

use std::sync::Arc;

use dashmap::DashMap;
use parking_lot::Mutex;

/// Defaults align with the paper (α=0.2) and the 1 s sampling window
/// suggested in `project_per_sae_fairness_design.md`.
pub const DEFAULT_ALPHA: f64 = 0.2;
pub const DEFAULT_WINDOW_MS: u64 = 1_000;

/// Smoothed counter for a single `(self, peer)` commodity. Holds the
/// in-flight window count plus the current EWMA. Wrapped in a
/// [`Mutex`] because a single peer is touched both by the request
/// handlers (record) and by the periodic reporter (rate).
#[derive(Debug)]
pub struct EwmaCounter {
    alpha: f64,
    window_ms: u64,
    state: Mutex<EwmaState>,
}

#[derive(Debug, Clone, Copy)]
struct EwmaState {
    ewma: f64,
    window_count: u64,
    window_start_ms: i64,
}

impl EwmaCounter {
    pub fn new(alpha: f64, window_ms: u64, now_ms: i64) -> Self {
        Self {
            alpha,
            window_ms: window_ms.max(1),
            state: Mutex::new(EwmaState {
                ewma: 0.0,
                window_count: 0,
                window_start_ms: now_ms,
            }),
        }
    }

    /// Add `count` keys to the in-flight window. If the call lands
    /// after one or more whole windows have elapsed, those windows
    /// get processed first (one EWMA update per window, each with
    /// the `window_count` that was active at the time).
    pub fn record(&self, count: u64, now_ms: i64) {
        let mut s = self.state.lock();
        self.advance(&mut s, now_ms);
        s.window_count = s.window_count.saturating_add(count);
    }

    /// Current smoothed rate in keys/s. Advances through any idle
    /// windows so callers see decay even if `record()` hasn't fired
    /// recently.
    pub fn rate(&self, now_ms: i64) -> f64 {
        let mut s = self.state.lock();
        self.advance(&mut s, now_ms);
        s.ewma
    }

    /// Walk `s` forward to `now_ms`, processing each fully-elapsed
    /// window. The in-flight window (the one still accumulating at
    /// `now_ms`) is *not* converted to a sample — that happens the
    /// next time `advance` crosses its boundary.
    fn advance(&self, s: &mut EwmaState, now_ms: i64) {
        let window_ms = self.window_ms as i64;
        while now_ms - s.window_start_ms >= window_ms {
            // Sample = (keys in this window) / (window length in s).
            let sample = s.window_count as f64 * 1_000.0 / self.window_ms as f64;
            s.ewma = s.ewma * (1.0 - self.alpha) + sample * self.alpha;
            s.window_start_ms += window_ms;
            s.window_count = 0;
        }
    }
}

/// Per-DKMS demand tracker. One [`EwmaCounter`] per peer DKMS,
/// allocated lazily on first `record()`. Shared between the request
/// handlers (which call `record`) and the periodic reporter (which
/// builds the `/demand` payload).
#[derive(Debug)]
pub struct DemandTracker {
    counters: DashMap<String, EwmaCounter>,
    alpha: f64,
    window_ms: u64,
}

impl Default for DemandTracker {
    fn default() -> Self {
        Self::new(DEFAULT_ALPHA, DEFAULT_WINDOW_MS)
    }
}

impl DemandTracker {
    pub fn new(alpha: f64, window_ms: u64) -> Self {
        Self {
            counters: DashMap::new(),
            alpha,
            window_ms,
        }
    }

    /// Record `count` keys demanded against `peer`. Cheap path:
    /// fetches (or creates) the per-peer counter and updates it.
    pub fn record(&self, peer: &str, count: u64, now_ms: i64) {
        if let Some(c) = self.counters.get(peer) {
            c.record(count, now_ms);
            return;
        }
        // Slow path: insert + record. `entry().or_insert_with` would
        // be one call but the lock is also fine since the slow path
        // only fires on first sight of a peer.
        let c = EwmaCounter::new(self.alpha, self.window_ms, now_ms);
        c.record(count, now_ms);
        self.counters.insert(peer.to_string(), c);
    }

    /// Read the EWMA-smoothed rate for `peer` (keys/s). Returns 0.0
    /// for peers that have never been recorded — useful for the
    /// reporter which iterates over the *known* peer set rather than
    /// only peers with traffic.
    pub fn rate(&self, peer: &str, now_ms: i64) -> f64 {
        self.counters
            .get(peer)
            .map(|c| c.rate(now_ms))
            .unwrap_or(0.0)
    }

    pub fn alpha(&self) -> f64 {
        self.alpha
    }

    pub fn window_ms(&self) -> u64 {
        self.window_ms
    }

    /// True iff no peer has ever recorded a sample.
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    /// All peer ids the tracker has seen at least once. Order is
    /// not deterministic.
    pub fn known_peers(&self) -> Vec<String> {
        self.counters.iter().map(|r| r.key().clone()).collect()
    }
}

/// Convenience alias used by [`crate::service::DkmsService`] and the
/// Generator to share the tracker without sprinkling `Arc<...>` at
/// every callsite.
pub type SharedDemandTracker = Arc<DemandTracker>;

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    /// Wall-clock time helper. Tests pass absolute `now_ms` rather
    /// than calling `chrono::Utc::now()` so they remain
    /// deterministic.
    const T0: i64 = 1_000_000;

    #[test]
    fn new_counter_is_zero() {
        let c = EwmaCounter::new(0.2, 1_000, T0);
        assert_eq!(c.rate(T0), 0.0);
    }

    #[test]
    fn record_in_same_window_doesnt_emit_sample_yet() {
        // No window has fully elapsed, so the EWMA hasn't taken its
        // first sample. This protects against a thundering-herd
        // burst inside one window from being mis-extrapolated as a
        // long-term rate.
        let c = EwmaCounter::new(0.2, 1_000, T0);
        c.record(50, T0 + 100);
        c.record(50, T0 + 500);
        assert_eq!(c.rate(T0 + 999), 0.0);
    }

    #[test]
    fn first_complete_window_yields_alpha_times_rate() {
        // 100 keys in 1s → sample = 100 kps. EWMA from 0 with α=0.2
        // jumps to 0 + 0.2 · 100 = 20.
        let c = EwmaCounter::new(0.2, 1_000, T0);
        c.record(100, T0 + 100);
        // Force a sample by advancing past the window boundary.
        let r = c.rate(T0 + 1_500);
        assert!((r - 20.0).abs() < 1e-9, "expected 20.0, got {r}");
    }

    #[test]
    fn steady_state_approaches_input_rate() {
        // Feed a constant 100 kps for ~30 windows; the EWMA should
        // converge to ~100. We don't expect exact equality — the
        // geometric series approaches but never touches the asymptote.
        let c = EwmaCounter::new(0.2, 1_000, T0);
        let mut t = T0;
        for _ in 0..30 {
            // 100 keys per second, spread over the window.
            c.record(100, t + 500);
            t += 1_000;
        }
        let r = c.rate(t);
        assert!(
            (r - 100.0).abs() < 1.0,
            "expected ≈100 kps after 30 windows, got {r}"
        );
    }

    #[test]
    fn idle_windows_decay_toward_zero() {
        // Charge up, then idle for many windows.
        let c = EwmaCounter::new(0.5, 1_000, T0);
        for i in 0..10 {
            c.record(100, T0 + i * 1_000 + 100);
        }
        let _ = c.rate(T0 + 10_000);
        // Now don't record for another 30 windows; rate should drop
        // sharply (α=0.5 decays fast).
        let r_late = c.rate(T0 + 10_000 + 30_000);
        assert!(
            r_late < 1.0,
            "expected near-zero after long idle, got {r_late}"
        );
    }

    #[test]
    fn record_with_zero_count_is_noop() {
        let c = EwmaCounter::new(0.2, 1_000, T0);
        c.record(0, T0 + 500);
        // Advance past the window — sample = 0 → EWMA still 0.
        assert_eq!(c.rate(T0 + 1_500), 0.0);
    }

    #[test]
    fn tracker_record_creates_counter_on_first_call() {
        let t = DemandTracker::default();
        assert!(t.is_empty());
        t.record("dkms-B", 50, T0);
        assert!(!t.is_empty());
        assert_eq!(t.known_peers(), vec!["dkms-B".to_string()]);
    }

    #[test]
    fn tracker_record_and_rate_per_peer() {
        let t = DemandTracker::new(0.2, 1_000);
        // 100 kps to B for 30 windows.
        let mut ts = T0;
        for _ in 0..30 {
            t.record("dkms-B", 100, ts + 500);
            ts += 1_000;
        }
        // 20 kps to C for 30 windows.
        let mut ts2 = T0;
        for _ in 0..30 {
            t.record("dkms-C", 20, ts2 + 500);
            ts2 += 1_000;
        }
        let r_b = t.rate("dkms-B", ts);
        let r_c = t.rate("dkms-C", ts2);
        assert!((r_b - 100.0).abs() < 1.0, "B: {r_b}");
        assert!((r_c - 20.0).abs() < 1.0, "C: {r_c}");
    }

    #[test]
    fn rate_for_unknown_peer_is_zero() {
        let t = DemandTracker::default();
        assert_eq!(t.rate("dkms-X", T0), 0.0);
    }

    #[test]
    fn known_peers_lists_every_peer_recorded() {
        let t = DemandTracker::default();
        t.record("dkms-B", 1, T0);
        t.record("dkms-C", 1, T0);
        t.record("dkms-B", 1, T0); // duplicate doesn't grow the set
        let mut peers = t.known_peers();
        peers.sort();
        assert_eq!(peers, vec!["dkms-B".to_string(), "dkms-C".to_string()]);
    }
}
