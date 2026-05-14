//! Decide whether a new demand fits on a link without violating its
//! advertised capacity. Conservative: refuses if granted_bps would exceed
//! `capacity_bps - safety_margin`.

use crate::topology::TopologyStore;

pub struct AdmissionDecision {
    pub allowed:    bool,
    pub granted_bps: u64,
    pub reason:     String,
}

pub fn check(_store: &TopologyStore, _link_id: &str, requested_bps: u64) -> AdmissionDecision {
    // TODO: read live usage from `CapacityReport` stream, subtract from
    // capacity, decide.
    AdmissionDecision {
        allowed:    true,
        granted_bps: requested_bps,
        reason:     "stub: unrestricted".into(),
    }
}
