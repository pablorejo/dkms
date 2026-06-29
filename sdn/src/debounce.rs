//! Recompute debouncer for the MCF solver.
//!
//! Port of the Python `RecomputeDebouncer` (`code_dkms/src/SDN/debounce.py`).
//!
//! ## What it does
//!
//! It coalesces a burst of `request()` calls into a single fire of
//! `recompute_mcf`. Each request slides the window forward; the
//! solver only runs once the input has been quiet for `window`
//! duration.
//!
//! ## Anti-starvation cap (`max_wait`)
//!
//! The naïve version of this — used in the early Python branch — had
//! a known failure mode: under a sustained request rate higher than
//! `1/window`, each new request resets the window timer, the timer
//! never fires, and the solver never runs. In the 50-DKMS cluster
//! that hit ~5 req/s with a 500 ms window, the MCF stopped applying
//! priority updates entirely.
//!
//! The fix is `max_wait`: track when the *current cycle* started
//! (i.e. the first request since the last fire). When a new request
//! comes in and the cycle has been open ≥ `max_wait`, fire
//! immediately and close the cycle. This guarantees the solver runs
//! at least once every `max_wait` seconds under any input rate while
//! still preserving the small-burst coalescing behaviour described
//! in the paper.
//!
//! ## Concurrency model
//!
//! State lives behind a [`parking_lot::Mutex`] (we never hold it
//! across an `await`). A single background worker task sleeps until
//! the next deadline, with a [`tokio::sync::Notify`] wake-up so
//! mutations can short-circuit the sleep. The fire callback is
//! dispatched via [`tokio::task::spawn_blocking`], so a multi-second
//! MCF solve never blocks the async runtime.

use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{trace, warn};

/// Boxed sync fire callback shared between the worker task and the
/// storm-fire path inside `request()`.
type FireFn = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Debug)]
struct State {
    window: Duration,
    max_wait: Duration,
    /// `true` between the first request of a cycle and the moment we
    /// fire (or cancel).
    pending: bool,
    /// When the first request of the current cycle landed. `None`
    /// outside a cycle.
    cycle_start: Option<Instant>,
    /// When the most recent request of the current cycle landed.
    /// Drives the sliding-window deadline.
    last_request: Option<Instant>,
    /// Once flipped on `shutdown()`, the worker exits at its next
    /// chance and no further fires are scheduled.
    closed: bool,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
    fire: FireFn,
    /// `true` while a fire callback is running on its blocking thread.
    /// Together with `fire_rerun` this caps the debouncer at ONE fire
    /// in flight: when the fire (an MCF solve) outlives the debounce
    /// window, overlapping fires would otherwise stack unbounded
    /// concurrent solves (er-n60: 10 simultaneous ~6-min solves of a
    /// 1.13M-variable LP starved the whole SDN node and every SAE
    /// binding lookup 404'd).
    fire_inflight: AtomicBool,
    /// Set when a fire was requested while one was in flight; the
    /// running fire re-runs once on completion so no trigger is lost.
    fire_rerun: AtomicBool,
}

#[derive(Clone)]
pub struct Debouncer {
    inner: Arc<Inner>,
}

impl Debouncer {
    /// Build a new debouncer and spawn its worker task on the
    /// current tokio runtime.
    ///
    /// * `window` — sliding quiet period after the most recent
    ///   request. Set to zero to make the debouncer fire on every
    ///   request (useful for tests).
    /// * `max_wait` — upper bound on cycle age. If a request arrives
    ///   with the current cycle older than this, fire is forced.
    /// * `fire` — runs on `tokio::task::spawn_blocking` whenever the
    ///   debouncer decides to fire.
    pub fn new<F>(window: Duration, max_wait: Duration, fire: F) -> Self
    where
        F: Fn() + Send + Sync + 'static,
    {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                window,
                max_wait,
                pending: false,
                cycle_start: None,
                last_request: None,
                closed: false,
            }),
            notify: Notify::new(),
            fire: Arc::new(fire),
            fire_inflight: AtomicBool::new(false),
            fire_rerun: AtomicBool::new(false),
        });
        let worker_inner = inner.clone();
        tokio::spawn(async move { run_worker(worker_inner).await });
        Self { inner }
    }

    /// Schedule a recompute. May fire immediately if the window is
    /// zero or if the current cycle has been open longer than
    /// `max_wait` (storm fire). The fire itself runs on a blocking
    /// task — `request()` returns promptly.
    pub fn request(&self) {
        let fire_now = {
            let mut s = self.inner.state.lock();
            if s.closed {
                trace!("debouncer: request after shutdown — dropping");
                return;
            }
            let now = Instant::now();

            // Window=0: deterministic immediate fire (useful in tests
            // and as a kill-switch via update_timing).
            if s.window.is_zero() {
                s.pending = false;
                s.cycle_start = None;
                s.last_request = None;
                true
            } else if let Some(cs) = s.cycle_start {
                if now.duration_since(cs) >= s.max_wait {
                    // Storm: cycle has been open longer than the cap.
                    // Close it and fire.
                    s.pending = false;
                    s.cycle_start = None;
                    s.last_request = None;
                    true
                } else {
                    s.pending = true;
                    s.last_request = Some(now);
                    false
                }
            } else {
                s.cycle_start = Some(now);
                s.pending = true;
                s.last_request = Some(now);
                false
            }
        };
        if fire_now {
            spawn_fire(&self.inner);
        } else {
            self.inner.notify.notify_one();
        }
    }

    /// Fire immediately if there's a pending recompute. No-op if the
    /// cycle is clean. Used by graceful shutdown.
    pub fn flush(&self) {
        let should_fire = {
            let mut s = self.inner.state.lock();
            if !s.pending {
                return;
            }
            s.pending = false;
            s.cycle_start = None;
            s.last_request = None;
            true
        };
        if should_fire {
            spawn_fire(&self.inner);
        }
    }

    /// Drop any pending fire without running it.
    pub fn cancel(&self) {
        {
            let mut s = self.inner.state.lock();
            s.pending = false;
            s.cycle_start = None;
            s.last_request = None;
        }
        self.inner.notify.notify_one();
    }

    /// Re-tune the timings at runtime. Used by the auto-tune after
    /// the topology is loaded and we know `N` of DKMSs.
    pub fn update_timing(&self, window: Duration, max_wait: Duration) {
        {
            let mut s = self.inner.state.lock();
            s.window = window;
            s.max_wait = max_wait;
        }
        self.inner.notify.notify_one();
    }

    /// Stop the worker task. Any pending fire is dropped.
    pub fn shutdown(&self) {
        {
            let mut s = self.inner.state.lock();
            s.closed = true;
            s.pending = false;
            s.cycle_start = None;
            s.last_request = None;
        }
        self.inner.notify.notify_one();
    }

    pub fn window(&self) -> Duration {
        self.inner.state.lock().window
    }

    pub fn max_wait(&self) -> Duration {
        self.inner.state.lock().max_wait
    }
}

fn spawn_fire(inner: &Arc<Inner>) {
    // At most ONE fire in flight. A fire requested while another runs
    // coalesces into a single re-run after it finishes — keeping the
    // debouncer's contract ("bursts → one solve") even when the solve
    // outlives the debounce window. A rerun flag set in the narrow gap
    // between the runner's final check and clearing `fire_inflight` is
    // picked up by the NEXT spawn_fire; during load those arrive
    // continuously, so staleness is bounded by one debounce cycle.
    if inner.fire_inflight.swap(true, Ordering::AcqRel) {
        inner.fire_rerun.store(true, Ordering::Release);
        return;
    }
    let inner = inner.clone();
    // spawn_blocking so a long MCF solve doesn't tie up a tokio worker.
    // We don't await the JoinHandle — fires are fire-and-forget. Bound
    // it with a name (vs `let _ = ...`) so clippy doesn't think we're
    // dropping an un-awaited future without spawning.
    let _handle = tokio::task::spawn_blocking(move || {
        loop {
            inner.fire_rerun.store(false, Ordering::Release);
            // Catch panics so a buggy fire fn can't kill the runtime.
            if let Err(payload) =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (inner.fire)()))
            {
                warn!(?payload, "debouncer fire panicked");
            }
            if !inner.fire_rerun.swap(false, Ordering::AcqRel) {
                break;
            }
            trace!("debouncer fire re-run (triggers coalesced during previous fire)");
        }
        inner.fire_inflight.store(false, Ordering::Release);
    });
}

async fn run_worker(inner: Arc<Inner>) {
    loop {
        // Snapshot what we need from state, then drop the lock before
        // any awaits.
        let next = {
            let s = inner.state.lock();
            if s.closed {
                return;
            }
            match (s.pending, s.last_request) {
                (true, Some(last)) => Wake::At(last + s.window),
                _ => Wake::Idle,
            }
        };
        match next {
            Wake::Idle => {
                inner.notify.notified().await;
            }
            Wake::At(deadline) => {
                tokio::select! {
                    _ = sleep_until(deadline) => {
                        // Deadline expired; fire if still pending and
                        // nobody has slid the window forward in the
                        // meantime.
                        let should_fire = {
                            let mut s = inner.state.lock();
                            if s.closed { return; }
                            match (s.pending, s.last_request) {
                                (true, Some(last)) if last + s.window <= Instant::now() => {
                                    s.pending = false;
                                    s.cycle_start = None;
                                    s.last_request = None;
                                    true
                                }
                                _ => false,
                            }
                        };
                        if should_fire {
                            spawn_fire(&inner);
                        }
                    }
                    _ = inner.notify.notified() => {
                        // State changed (request slid the window, or
                        // update_timing/cancel/shutdown was called).
                        // Loop and re-snapshot.
                    }
                }
            }
        }
    }
}

enum Wake {
    Idle,
    At(Instant),
}

async fn sleep_until(deadline: Instant) {
    let now = Instant::now();
    if deadline <= now {
        return;
    }
    tokio::time::sleep(deadline - now).await;
}

// ---------------------------------------------------------------- helpers
// Exposed as free functions (rather than methods) so callers can compute
// the timings without owning a Debouncer instance.

/// Estimate the cost of one MCF solve and pick sensible debouncer
/// timings based on it. Direct port of the Python
/// `estimate_debounce_timing`. Returns
/// `(est_solve, window, max_wait)`.
///
/// Empirical calibration from the 50-DKMS cluster:
/// `est_solve_s = max(0.5, (N/50)³ · 8)`. `max_wait` is `solve·1.25 + 1s`
/// (25% margin + 1s slack for GC). `window` is 20% of `max_wait` with a
/// 0.5s floor.
pub fn estimate_timing(n_dkms: usize) -> (Duration, Duration, Duration) {
    if n_dkms == 0 {
        return (
            Duration::from_millis(500),
            Duration::from_millis(500),
            Duration::from_secs(2),
        );
    }
    let n = n_dkms as f64;
    let est_solve_s = ((n / 50.0).powi(3) * 8.0).max(0.5);
    let max_wait_s = (est_solve_s * 1.25 + 1.0).max(2.0);
    let window_s = (max_wait_s * 0.2).max(0.5);
    (
        Duration::from_secs_f64(est_solve_s),
        Duration::from_secs_f64(window_s),
        Duration::from_secs_f64(max_wait_s),
    )
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::sleep;

    fn counter() -> (Arc<AtomicUsize>, impl Fn() + Send + Sync + 'static) {
        let c = Arc::new(AtomicUsize::new(0));
        let cc = c.clone();
        (c, move || {
            cc.fetch_add(1, Ordering::SeqCst);
        })
    }

    /// Wait for a counter to reach `target` (or fail after `bound`).
    async fn wait_for(c: &AtomicUsize, target: usize, bound: Duration) {
        let start = Instant::now();
        loop {
            if c.load(Ordering::SeqCst) >= target {
                return;
            }
            if start.elapsed() > bound {
                panic!(
                    "counter only reached {} (expected {}) in {:?}",
                    c.load(Ordering::SeqCst),
                    target,
                    bound,
                );
            }
            sleep(Duration::from_millis(2)).await;
        }
    }

    #[tokio::test]
    async fn single_request_fires_after_window() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(40), Duration::from_secs(1), fire);
        d.request();
        // Hasn't fired yet (sliding window hasn't elapsed).
        assert_eq!(c.load(Ordering::SeqCst), 0);
        wait_for(&c, 1, Duration::from_millis(200)).await;
        // Should be exactly one fire.
        sleep(Duration::from_millis(80)).await;
        assert_eq!(c.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rapid_requests_coalesce_into_one_fire() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(40), Duration::from_secs(10), fire);
        for _ in 0..20 {
            d.request();
            sleep(Duration::from_millis(5)).await;
        }
        // While requests keep coming with gap < window, no fire.
        assert_eq!(c.load(Ordering::SeqCst), 0);
        wait_for(&c, 1, Duration::from_millis(200)).await;
        sleep(Duration::from_millis(80)).await;
        assert_eq!(c.load(Ordering::SeqCst), 1, "exactly one coalesced fire");
    }

    #[tokio::test]
    async fn storm_force_fires_after_max_wait() {
        let (c, fire) = counter();
        // Window > max_wait so sliding alone would never fire while we
        // keep requesting.
        let d = Debouncer::new(Duration::from_millis(80), Duration::from_millis(40), fire);
        let start = Instant::now();
        while start.elapsed() < Duration::from_millis(150) {
            d.request();
            sleep(Duration::from_millis(5)).await;
        }
        // At least one storm fire while requests were arriving rapidly.
        wait_for(&c, 1, Duration::from_millis(100)).await;
        assert!(c.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn zero_window_fires_immediately() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::ZERO, Duration::from_secs(1), fire);
        d.request();
        wait_for(&c, 1, Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn cancel_drops_pending_fire() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(40), Duration::from_secs(1), fire);
        d.request();
        d.cancel();
        sleep(Duration::from_millis(80)).await;
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn flush_fires_pending_immediately() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(500), Duration::from_secs(1), fire);
        d.request();
        // Sliding-window deadline is 500ms away — flush() should not wait.
        let t = Instant::now();
        d.flush();
        wait_for(&c, 1, Duration::from_millis(100)).await;
        assert!(t.elapsed() < Duration::from_millis(150));
    }

    #[tokio::test]
    async fn flush_with_nothing_pending_is_noop() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(40), Duration::from_secs(1), fire);
        d.flush();
        sleep(Duration::from_millis(80)).await;
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn update_timing_changes_deadline() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_secs(10), Duration::from_secs(60), fire);
        d.request();
        // The original window would have made us wait 10s. Shrink it.
        d.update_timing(Duration::from_millis(30), Duration::from_secs(1));
        // The next request restarts the cycle under the new window.
        d.request();
        wait_for(&c, 1, Duration::from_millis(200)).await;
    }

    #[tokio::test]
    async fn shutdown_stops_future_fires() {
        let (c, fire) = counter();
        let d = Debouncer::new(Duration::from_millis(40), Duration::from_secs(1), fire);
        d.shutdown();
        d.request();
        sleep(Duration::from_millis(80)).await;
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn estimate_timing_scales_with_n() {
        let (s10, w10, mw10) = estimate_timing(10);
        let (s50, w50, mw50) = estimate_timing(50);
        // Larger N → longer estimated solve and longer windows.
        assert!(s50 >= s10);
        assert!(w50 >= w10);
        assert!(mw50 >= mw10);
        // Floors hold.
        assert!(w10 >= Duration::from_millis(500));
        assert!(mw10 >= Duration::from_secs(2));
        // N=50 should give ~8s solve (the calibration point).
        assert!(s50 >= Duration::from_secs_f64(7.9));
        assert!(s50 <= Duration::from_secs_f64(8.1));
    }

    #[test]
    fn estimate_timing_handles_zero() {
        let (s, w, mw) = estimate_timing(0);
        assert_eq!(s, Duration::from_millis(500));
        assert_eq!(w, Duration::from_millis(500));
        assert_eq!(mw, Duration::from_secs(2));
    }
}
