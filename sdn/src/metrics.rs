//! SDN Prometheus metric handles.
//!
//! Registers a small set of counters/histograms with the SDN's
//! [`common::metrics::Metrics`] registry. The handles are cloned into
//! the [`crate::service::SdnService`] so any code path that ingests a
//! demand POST or runs the LP solver can emit without going through
//! the registry on the hot path.
//!
//! Why this exists: the v13-validation campaign showed 17-33 % of
//! `/demand` POSTs were silently dropping at the transport layer
//! during SAE ramps, and we had no way to see it from outside the
//! DKMS logs. With these counters in place, an operator can
//! `kubectl port-forward sdn-<id> 9100:9100 && curl /metrics` and
//! see the rates plus LP solve duration in real time.

use common::metrics::Metrics;
use prometheus::{Histogram, HistogramOpts, IntCounterVec, Opts};

/// Registered Prometheus metric handles for the SDN.
#[derive(Clone)]
pub struct SdnMetrics {
    /// `sdn_demand_post_total{outcome}` — count of `POST /demand`
    /// terminations. `outcome` ∈ `{ok, partial, bad_request, error}`.
    /// `bad_request` matches the 400 path inside `post_demand`;
    /// `error` is reserved for server-side panics caught by the
    /// handler (currently unused, but kept for parity).
    pub demand_post_total: IntCounterVec,

    /// `sdn_demand_post_duration_seconds` — wall time spent inside
    /// `post_demand` from the moment the JSON body has been parsed to
    /// the moment the response is built. Tail-latency on this is the
    /// SDN-side signal of why a DKMS `post_demand` call may have
    /// timed out client-side.
    pub demand_post_duration_seconds: Histogram,

    /// `sdn_lp_solve_duration_seconds` — wall time of one
    /// `recompute_mcf_inner` call (build inputs + microlp + adapt).
    /// Tells the operator whether the LP is keeping up with the
    /// demand-update cadence (`mcf_period_ms`).
    pub lp_solve_duration_seconds: Histogram,
}

impl SdnMetrics {
    /// Register every handle against `metrics.registry`. On
    /// duplicate-registration error (only possible if `register` is
    /// called twice with the same registry) this panics — there is
    /// no recovery path that makes sense, and the alternative would
    /// be silently exposing only some of the metrics.
    pub fn register(metrics: &Metrics) -> Self {
        let demand_post_total = IntCounterVec::new(
            Opts::new(
                "sdn_demand_post_total",
                "Total POST /demand calls received by the SDN, by outcome",
            ),
            &["outcome"],
        )
        .expect("sdn_demand_post_total opts");
        let demand_post_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "sdn_demand_post_duration_seconds",
                "Wall time of POST /demand handler (seconds)",
            )
            // Buckets chosen for the expected operating range: <1 ms
            // happy path up to 5 s (around the DKMS `rpc_timeout_ms`
            // ceiling). Anything above 5 s is the client timeout
            // anyway.
            .buckets(vec![
                0.0005, 0.001, 0.002, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0,
            ]),
        )
        .expect("sdn_demand_post_duration_seconds opts");
        let lp_solve_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "sdn_lp_solve_duration_seconds",
                "Wall time of one MCMCF-λ LP solve (seconds)",
            )
            // Empirically microlp at N=20 is 100 ms-ish and at N=40
            // climbs to 30-60 s before OOM. The buckets cover both
            // happy path and pathological cases without losing
            // resolution near the operating point.
            .buckets(vec![
                0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
            ]),
        )
        .expect("sdn_lp_solve_duration_seconds opts");

        metrics
            .registry
            .register(Box::new(demand_post_total.clone()))
            .expect("register demand_post_total");
        metrics
            .registry
            .register(Box::new(demand_post_duration_seconds.clone()))
            .expect("register demand_post_duration_seconds");
        metrics
            .registry
            .register(Box::new(lp_solve_duration_seconds.clone()))
            .expect("register lp_solve_duration_seconds");

        // Pre-warm the known outcome labels so `/metrics` shows "0"
        // for every outcome from boot rather than the metric only
        // appearing once the first request of that outcome lands.
        // (prometheus IntCounterVec hides the whole family until any
        // label is touched.)
        for outcome in ["ok", "partial", "bad_request", "error"] {
            demand_post_total.with_label_values(&[outcome]).inc_by(0);
        }

        Self {
            demand_post_total,
            demand_post_duration_seconds,
            lp_solve_duration_seconds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_emits_three_families() {
        let m = Metrics::new("sdn-test");
        let _ = SdnMetrics::register(&m);
        let families = m.registry.gather();
        let names: Vec<&str> = families.iter().map(|f| f.get_name()).collect();
        assert!(names.contains(&"sdn_demand_post_total"));
        assert!(names.contains(&"sdn_demand_post_duration_seconds"));
        assert!(names.contains(&"sdn_lp_solve_duration_seconds"));
    }

    #[test]
    fn outcome_label_increments_independently() {
        let m = Metrics::new("sdn-test");
        let sm = SdnMetrics::register(&m);
        sm.demand_post_total.with_label_values(&["ok"]).inc();
        sm.demand_post_total.with_label_values(&["ok"]).inc();
        sm.demand_post_total.with_label_values(&["partial"]).inc();
        assert_eq!(sm.demand_post_total.with_label_values(&["ok"]).get(), 2);
        assert_eq!(
            sm.demand_post_total.with_label_values(&["partial"]).get(),
            1
        );
    }
}
