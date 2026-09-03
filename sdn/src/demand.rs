//! Per-commodity demand registry consumed by the MCMCF-λ solver.
//!
//! A *commodity* is an ordered pair `(src_dkms, dst_dkms)` — the same
//! granularity the legacy `mcf::Commodity` already uses. Each DKMS owns
//! the demand it generates as the *source* of a commodity (it knows its
//! own buffer level, capacity, and the rate at which SAEs upstream of
//! it drain that buffer). DKMSs POST their snapshot to `/demand`
//! periodically; the SDN keeps the latest report per `(src, dst)` pair
//! and the solver reads it on each recompute.
//!
//! Paper mapping (`docs/mcmcf-lambda.tex`):
//!
//! | Field        | Paper symbol | Meaning                          |
//! |--------------|--------------|----------------------------------|
//! | `level`      | `L_k`        | current buffer level (keys)      |
//! | `capacity`   | `B_k`        | buffer capacity (keys)           |
//! | `drain_rate` | `δ_k`        | SAE consumption rate (keys/s)    |
//!
//! The solver derives `R_k = B_k − L_k` (remaining space) at solve time;
//! we store the raw `(L_k, B_k)` pair so the registry doesn't have to
//! be invalidated if the buffer capacity changes mid-flight.
//!
//! ## Why per-commodity, not per-DKMS?
//!
//! A single DKMS has N−1 outgoing buffers (one per peer) and may see
//! different drain rates on each. SAEs upstream of `A` requesting keys
//! whose peer is `B` only drain the `(A, B)` buffer, not `(A, C)`.
//! Aggregating to per-DKMS would lose this discrimination.

use std::sync::Arc;

use common::security::KeyGrade;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Demand snapshot for a single `(src_dkms, dst_dkms)` commodity.
///
/// All rates are in keys/second; levels and capacities are in keys.
/// Negative values are rejected at the HTTP boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommodityDemand {
    /// Source DKMS — the one generating keys into the buffer.
    pub src_dkms: String,

    /// Destination DKMS — the peer whose buffer is being filled.
    pub dst_dkms: String,

    /// Current buffer level (keys). `0 ≤ level ≤ capacity`.
    pub level: f64,

    /// Buffer capacity (keys). `> 0`.
    pub capacity: f64,

    /// Consumption rate from SAE side, EWMA-smoothed (keys/s). `≥ 0`.
    /// In paper notation this is `δ_k`.
    pub drain_rate: f64,

    /// Wall-clock when the source DKMS sampled this record. Used by
    /// the solver to drop stale entries (e.g., a peer DKMS that
    /// crashed and stopped reporting).
    pub timestamp_ms: i64,

    /// Security grade this demand must be served at: [`KeyGrade::Qkd`] routes
    /// only over QKD arcs, [`KeyGrade::Pqc`] over any arc. `#[serde(default)]`
    /// → reports from a DKMS that predates security levels deserialize as
    /// `Qkd`. The solver currently overrides this from QKD-subgraph
    /// connectivity (see `McmcfInputs::build`); once the DKMS splits demand
    /// per grade this field carries the real per-grade split.
    #[serde(default)]
    pub grade: KeyGrade,
}

/// Reloj de pared en ms desde epoch (para clamp de timestamps de demanda).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl CommodityDemand {
    /// Remaining buffer space `R_k = B_k − L_k`. Always non-negative;
    /// clamped to 0 if a stale report has `level > capacity`.
    #[inline]
    pub fn remaining(&self) -> f64 {
        (self.capacity - self.level).max(0.0)
    }

    /// Sanity check: fields are within plausible bounds.
    pub fn is_well_formed(&self) -> bool {
        // Magnitudes acotadas (auditoría 2026-09-03, C-05): un `drain_rate`
        // de 1e300 finito y positivo pasaba, y la proyección a factible
        // (`out[i] = xs[i] / worst`) dejaba a CERO toda commodity que
        // compartiera arista con él — justo el `starved = 0` que el
        // asignador existe para garantizar.
        const MAX_MAGNITUDE: f64 = 1.0e9;
        self.capacity > 0.0
            && self.level >= 0.0
            && self.drain_rate >= 0.0
            && self.level.is_finite()
            && self.capacity.is_finite()
            && self.drain_rate.is_finite()
            && self.capacity <= MAX_MAGNITUDE
            && self.level <= MAX_MAGNITUDE
            && self.drain_rate <= MAX_MAGNITUDE
            && !self.src_dkms.is_empty()
            && !self.dst_dkms.is_empty()
            && self.src_dkms != self.dst_dkms
    }
}

/// Wire format of `POST /demand`. A single DKMS reports every
/// commodity it sources in one batched call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DemandReport {
    /// The DKMS that owns this batch — `entries[i].src_dkms` must
    /// equal this. Cross-checked at intake; mismatches are rejected.
    pub dkms_id: String,
    /// One entry per outgoing commodity from `dkms_id`.
    pub entries: Vec<CommodityDemand>,
}

/// Result of recording a single report — separated from `Result<>` so
/// we can return partial-success (some entries accepted, some rejected).
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct IngestSummary {
    /// Entries persisted to the registry.
    pub accepted: usize,
    /// Reasons for rejection, one per bad entry. Limit 8 in the
    /// response to keep error bodies bounded.
    pub errors: Vec<String>,
}

/// Concurrent registry keyed by `(src_dkms, dst_dkms)`. Stores the
/// *latest* report per commodity; second reports replace the first.
///
/// Wrapped in [`Arc`] in [`crate::service::SdnService`] so background
/// recomputes and HTTP intake share the same instance without locks
/// on the hot path.
#[derive(Debug, Default)]
pub struct DemandRegistry {
    /// `(src_dkms, dst_dkms, grade) -> latest CommodityDemand`. Keyed by grade
    /// too so a DKMS can report distinct demand for QKD-grade vs PQC-grade
    /// traffic on the same pair (e.g. `strict_qkd` and `no_worry` SAEs).
    inner: DashMap<(String, String, KeyGrade), CommodityDemand>,
}

impl DemandRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Ingest a full DKMS report. Returns a summary with accepted and
    /// rejected counts. Rejected entries are *not* a global failure —
    /// the well-formed ones are still persisted.
    pub fn ingest(&self, report: DemandReport) -> IngestSummary {
        let mut s = IngestSummary::default();
        for entry in report.entries.into_iter() {
            if entry.src_dkms != report.dkms_id {
                s.push_error(format!(
                    "entry src_dkms={} does not match report dkms_id={}",
                    entry.src_dkms, report.dkms_id
                ));
                continue;
            }
            if !entry.is_well_formed() {
                s.push_error(format!(
                    "malformed entry {}→{}",
                    entry.src_dkms, entry.dst_dkms
                ));
                continue;
            }
            // Clamp del timestamp al reloj local (auditoría 2026-09b B11): un
            // `timestamp_ms` futuro elegido por el reportero hacía la entrada
            // inmortal (`evict_older_than` compara contra `now - max_age`).
            let mut entry = entry;
            let now = now_ms();
            if entry.timestamp_ms > now {
                entry.timestamp_ms = now;
            }
            let key = (entry.src_dkms.clone(), entry.dst_dkms.clone(), entry.grade);
            self.inner.insert(key, entry);
            s.accepted += 1;
        }
        s
    }

    /// Look up a single commodity by pair **and grade**. Returns `None` if no
    /// DKMS has reported for that (pair, grade) yet.
    pub fn get(&self, src_dkms: &str, dst_dkms: &str, grade: KeyGrade) -> Option<CommodityDemand> {
        self.inner
            .get(&(src_dkms.to_string(), dst_dkms.to_string(), grade))
            .map(|r| r.value().clone())
    }

    /// All known commodities, materialised as a `Vec` so the caller
    /// can hold it without locking the map.
    pub fn snapshot(&self) -> Vec<CommodityDemand> {
        self.inner.iter().map(|r| r.value().clone()).collect()
    }

    /// Number of (src, dst) pairs with at least one report on record.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Drop entries older than `max_age_ms` relative to `now_ms`.
    /// Returns the number of entries evicted. Called from the presence
    /// sweeper (`service.rs`) every `presence_ttl_secs / 3` with
    /// `demand_ttl_secs` as the age.
    pub fn evict_older_than(&self, now_ms: i64, max_age_ms: i64) -> usize {
        let cutoff = now_ms.saturating_sub(max_age_ms);
        let stale: Vec<_> = self
            .inner
            .iter()
            .filter(|r| r.value().timestamp_ms < cutoff)
            .map(|r| r.key().clone())
            .collect();
        let n = stale.len();
        for k in stale {
            self.inner.remove(&k);
        }
        n
    }
}

impl IngestSummary {
    fn push_error(&mut self, msg: String) {
        if self.errors.len() < 8 {
            self.errors.push(msg);
        }
    }
}

/// Type alias used by [`crate::service::SdnService`] to share the
/// registry between the HTTP handler and the solver.
pub type SharedDemandRegistry = Arc<DemandRegistry>;

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    fn demand(src: &str, dst: &str, level: f64, cap: f64, drain: f64, ts: i64) -> CommodityDemand {
        CommodityDemand {
            src_dkms: src.into(),
            dst_dkms: dst.into(),
            level,
            capacity: cap,
            drain_rate: drain,
            timestamp_ms: ts,
            grade: Default::default(),
        }
    }

    #[test]
    fn remaining_is_capacity_minus_level() {
        let d = demand("A", "B", 1000.0, 4096.0, 50.0, 0);
        assert!((d.remaining() - 3096.0).abs() < 1e-9);
    }

    #[test]
    fn remaining_clamps_to_zero_when_overfull() {
        // Stale report: level momentarily above capacity. Clamp to 0
        // instead of returning negative space to the solver.
        let d = demand("A", "B", 5000.0, 4096.0, 0.0, 0);
        assert_eq!(d.remaining(), 0.0);
    }

    #[test]
    fn well_formed_accepts_valid_record() {
        let d = demand("A", "B", 100.0, 4096.0, 30.0, 0);
        assert!(d.is_well_formed());
    }

    #[test]
    fn well_formed_rejects_self_loop_and_empty_ids() {
        assert!(!demand("A", "A", 0.0, 100.0, 0.0, 0).is_well_formed());
        assert!(!demand("", "B", 0.0, 100.0, 0.0, 0).is_well_formed());
        assert!(!demand("A", "", 0.0, 100.0, 0.0, 0).is_well_formed());
    }

    #[test]
    fn well_formed_rejects_non_finite_or_negative() {
        assert!(!demand("A", "B", -1.0, 100.0, 0.0, 0).is_well_formed());
        assert!(!demand("A", "B", 0.0, 0.0, 0.0, 0).is_well_formed());
        assert!(!demand("A", "B", 0.0, -1.0, 0.0, 0).is_well_formed());
        assert!(!demand("A", "B", 0.0, 100.0, -0.1, 0).is_well_formed());
        assert!(!demand("A", "B", f64::NAN, 100.0, 0.0, 0).is_well_formed());
        assert!(!demand("A", "B", 0.0, f64::INFINITY, 0.0, 0).is_well_formed());
    }

    #[test]
    fn ingest_accepts_and_lookup_returns_latest() {
        let reg = DemandRegistry::new();
        let r1 = DemandReport {
            dkms_id: "A".into(),
            entries: vec![
                demand("A", "B", 100.0, 4096.0, 30.0, 1000),
                demand("A", "C", 200.0, 4096.0, 40.0, 1000),
            ],
        };
        let s = reg.ingest(r1);
        assert_eq!(s.accepted, 2);
        assert!(s.errors.is_empty());
        assert_eq!(reg.len(), 2);
        let got = reg.get("A", "B", KeyGrade::Qkd).unwrap();
        assert_eq!(got.level, 100.0);
        assert_eq!(got.drain_rate, 30.0);
        assert!(reg.get("A", "Z", KeyGrade::Qkd).is_none());
    }

    #[test]
    fn future_timestamp_is_clamped_so_it_can_expire() {
        let reg = DemandRegistry::new();
        let s = reg.ingest(DemandReport {
            dkms_id: "A".into(),
            entries: vec![demand("A", "B", 1.0, 10.0, 1.0, i64::MAX)],
        });
        assert_eq!(s.accepted, 1);
        // Si el timestamp i64::MAX se hubiera guardado tal cual, no expiraría
        // jamás. Clampeado a ~now, expira con un `now` posterior (B11).
        let evicted = reg.evict_older_than(now_ms() + 10_000, 0);
        assert_eq!(
            evicted, 1,
            "un timestamp futuro clampeado debe poder expirar"
        );
    }

    #[test]
    fn ingest_replaces_on_second_report() {
        let reg = DemandRegistry::new();
        reg.ingest(DemandReport {
            dkms_id: "A".into(),
            entries: vec![demand("A", "B", 100.0, 4096.0, 30.0, 1000)],
        });
        reg.ingest(DemandReport {
            dkms_id: "A".into(),
            entries: vec![demand("A", "B", 200.0, 4096.0, 50.0, 2000)],
        });
        assert_eq!(reg.len(), 1);
        let got = reg.get("A", "B", KeyGrade::Qkd).unwrap();
        assert_eq!(got.level, 200.0);
        assert_eq!(got.drain_rate, 50.0);
        assert_eq!(got.timestamp_ms, 2000);
    }

    #[test]
    fn ingest_rejects_src_mismatch_but_keeps_good_entries() {
        let reg = DemandRegistry::new();
        let r = DemandReport {
            dkms_id: "A".into(),
            entries: vec![
                demand("A", "B", 100.0, 4096.0, 30.0, 1000),
                // Pretends to be from A but the entry says B → rejected.
                demand("B", "C", 100.0, 4096.0, 30.0, 1000),
                demand("A", "C", 200.0, 4096.0, 40.0, 1000),
            ],
        };
        let s = reg.ingest(r);
        assert_eq!(s.accepted, 2);
        assert_eq!(s.errors.len(), 1);
        assert_eq!(reg.len(), 2);
        assert!(reg.get("B", "C", KeyGrade::Qkd).is_none());
    }

    #[test]
    fn ingest_drops_malformed_entries() {
        let reg = DemandRegistry::new();
        let r = DemandReport {
            dkms_id: "A".into(),
            entries: vec![
                demand("A", "B", 100.0, 4096.0, 30.0, 1000),
                demand("A", "A", 100.0, 4096.0, 30.0, 1000), // self-loop
                demand("A", "C", -1.0, 4096.0, 30.0, 1000),  // negative level
            ],
        };
        let s = reg.ingest(r);
        assert_eq!(s.accepted, 1);
        assert_eq!(s.errors.len(), 2);
    }

    #[test]
    fn error_list_caps_at_eight() {
        let reg = DemandRegistry::new();
        let bad: Vec<_> = (0..20)
            .map(|_| demand("A", "A", 0.0, 100.0, 0.0, 0))
            .collect();
        let r = DemandReport {
            dkms_id: "A".into(),
            entries: bad,
        };
        let s = reg.ingest(r);
        assert_eq!(s.accepted, 0);
        assert_eq!(s.errors.len(), 8);
    }

    #[test]
    fn snapshot_returns_all_entries() {
        let reg = DemandRegistry::new();
        reg.ingest(DemandReport {
            dkms_id: "A".into(),
            entries: vec![
                demand("A", "B", 100.0, 4096.0, 30.0, 1000),
                demand("A", "C", 100.0, 4096.0, 30.0, 1000),
            ],
        });
        reg.ingest(DemandReport {
            dkms_id: "B".into(),
            entries: vec![demand("B", "A", 100.0, 4096.0, 30.0, 1000)],
        });
        let mut snap = reg.snapshot();
        snap.sort_by(|a, b| {
            (a.src_dkms.as_str(), a.dst_dkms.as_str())
                .cmp(&(b.src_dkms.as_str(), b.dst_dkms.as_str()))
        });
        assert_eq!(snap.len(), 3);
        assert_eq!(snap[0].src_dkms, "A");
        assert_eq!(snap[0].dst_dkms, "B");
    }

    #[test]
    fn evict_older_than_drops_stale() {
        let reg = DemandRegistry::new();
        reg.ingest(DemandReport {
            dkms_id: "A".into(),
            entries: vec![
                demand("A", "B", 0.0, 100.0, 0.0, 1000),
                demand("A", "C", 0.0, 100.0, 0.0, 9000),
            ],
        });
        // now=10000, max_age=5000 → cutoff=5000 → A→B (ts=1000) evicted.
        let n = reg.evict_older_than(10_000, 5_000);
        assert_eq!(n, 1);
        assert!(reg.get("A", "B", KeyGrade::Qkd).is_none());
        assert!(reg.get("A", "C", KeyGrade::Qkd).is_some());
    }
}
