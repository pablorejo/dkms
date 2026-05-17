//! Traffic priorities + buffer-priority registry.
//!
//! Mirror of the Python `link_admission.TrafficPriority` /
//! `BufferRole` enums and `topology.BufferPriorityRegistry`.
//!
//! ## Semantics
//!
//! Every DKMS↔DKMS flow has two buffers per peer: `ENC` (keys this
//! DKMS produces and pushes out) and `DEC` (keys it receives from the
//! peer). Each buffer has a priority class declared by the DKMS via
//! `PATCH /flows/{flow_id}/class`. The SDN keeps that classification
//! here and uses it as the weight input to the MCF solver.
//!
//! The default class is [`TrafficPriority::Priority`] (the *highest*),
//! not `BestEffort`. The reason — copied from the Python comment — is
//! to avoid a starvation loop at startup: a brand-new buffer hasn't
//! seen traffic yet, the DKMS hasn't notified anything, the registry
//! falls back to a class, and that class drives the MCF rate. If the
//! fallback were `BestEffort` (w=1) and a peer was `Priority`
//! (w=100), the new buffer would never get rate, would stay empty,
//! the DKMS wouldn't notify anything, and the loop would latch shut.
//! Defaulting to `Priority` lets the LP open up symmetrically and
//! traffic shapes itself once DKMSs start reporting saturation.
//!
//! ## Weights
//!
//! Exponential spread (10×) between consecutive classes — combined with
//! the weighted max-min solver, this gives Priority ≈10× the rate of
//! Important on the same edge, ≈100× BestEffort, etc., without ever
//! starving a class to 0. `Saturated` keeps weight 0: those flows are
//! excluded from the LP entirely and the orchestrator pins their rate
//! to 0 in the snapshot.

use std::str::FromStr;

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Priority class of a buffer. Six classes total — five active
/// (Priority → BestEffort) plus a terminal Saturated state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrafficPriority {
    Priority,
    Important,
    Quickly,
    Relax,
    BestEffort,
    Saturated,
}

impl TrafficPriority {
    /// Weight handed to the MCF solver. Class with weight 0 is
    /// excluded from the LP and pinned to rate 0 by the orchestrator.
    pub fn weight(self) -> f64 {
        match self {
            Self::Priority => 10_000.0,
            Self::Important => 1_000.0,
            Self::Quickly => 100.0,
            Self::Relax => 10.0,
            Self::BestEffort => 1.0,
            Self::Saturated => 0.0,
        }
    }

    /// Ordering — 1 = highest priority. Used by external code that
    /// wants a comparable scalar (e.g. UI sort).
    pub fn rank(self) -> u8 {
        match self {
            Self::Priority => 1,
            Self::Important => 2,
            Self::Quickly => 3,
            Self::Relax => 4,
            Self::BestEffort => 5,
            Self::Saturated => 6,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Important => "important",
            Self::Quickly => "quickly",
            Self::Relax => "relax",
            Self::BestEffort => "best_effort",
            Self::Saturated => "saturated",
        }
    }
}

impl FromStr for TrafficPriority {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "priority" => Ok(Self::Priority),
            "important" => Ok(Self::Important),
            "quickly" => Ok(Self::Quickly),
            "relax" => Ok(Self::Relax),
            "best_effort" => Ok(Self::BestEffort),
            "saturated" => Ok(Self::Saturated),
            other => Err(format!("invalid priority: {other}")),
        }
    }
}

/// Role of a DKMS buffer with respect to a peer DKMS.
///
/// For a flow `A → B`:
/// * `EncKeys` on `A` = what A pushes to B.
/// * `DecKeys` on `B` = what B receives from A.
///
/// Flows `A → B` and `B → A` are independent commodities with
/// independent priorities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BufferRole {
    EncKeys,
    DecKeys,
}

impl BufferRole {
    pub fn as_str(self) -> &'static str {
        match self {
            BufferRole::EncKeys => "enc_keys",
            BufferRole::DecKeys => "dec_keys",
        }
    }
}

impl FromStr for BufferRole {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "enc_keys" => Ok(BufferRole::EncKeys),
            "dec_keys" => Ok(BufferRole::DecKeys),
            other => Err(format!("invalid buffer role: {other}")),
        }
    }
}

/// Thread-safe registry mapping `(dkms, peer, role) → TrafficPriority`.
///
/// The orchestrator reads from it once per recompute to derive the
/// per-commodity weight; the HTTP layer writes to it whenever a DKMS
/// notifies a class change (`PATCH /flows/{flow_id}/class`).
#[derive(Debug, Default)]
pub struct BufferPriorityRegistry {
    states: RwLock<HashMap<(String, String, BufferRole), TrafficPriority>>,
}

impl BufferPriorityRegistry {
    pub fn new() -> Self {
        Self {
            states: RwLock::new(HashMap::new()),
        }
    }

    /// Set the priority of a buffer. Returns the previous value, if any.
    pub fn set(
        &self,
        dkms_id: &str,
        peer_dkms_id: &str,
        role: BufferRole,
        priority: TrafficPriority,
    ) -> Option<TrafficPriority> {
        let mut g = self.states.write();
        g.insert((dkms_id.into(), peer_dkms_id.into(), role), priority)
    }

    /// Read the priority. Defaults to [`TrafficPriority::Priority`]
    /// when no entry exists — see the type-level docs for *why*.
    pub fn get(&self, dkms_id: &str, peer_dkms_id: &str, role: BufferRole) -> TrafficPriority {
        self.states
            .read()
            .get(&(dkms_id.into(), peer_dkms_id.into(), role))
            .copied()
            .unwrap_or(TrafficPriority::Priority)
    }

    /// Empty the registry. Used by tests and by full topology reloads.
    pub fn clear(&self) {
        self.states.write().clear();
    }

    /// Snapshot the registry — for debug endpoints / introspection.
    pub fn snapshot(&self) -> Vec<((String, String, BufferRole), TrafficPriority)> {
        self.states
            .read()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }
}

// ----------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_are_decade_spaced() {
        // Decade spacing so the weighted max-min solver gives each class
        // ≈10× the rate of the one below on a shared edge.
        assert_eq!(TrafficPriority::Priority.weight(), 10_000.0);
        assert_eq!(TrafficPriority::Important.weight(), 1_000.0);
        assert_eq!(TrafficPriority::Quickly.weight(), 100.0);
        assert_eq!(TrafficPriority::Relax.weight(), 10.0);
        assert_eq!(TrafficPriority::BestEffort.weight(), 1.0);
        assert_eq!(TrafficPriority::Saturated.weight(), 0.0);
    }

    #[test]
    fn parse_round_trip() {
        for &p in &[
            TrafficPriority::Priority,
            TrafficPriority::Important,
            TrafficPriority::Quickly,
            TrafficPriority::Relax,
            TrafficPriority::BestEffort,
            TrafficPriority::Saturated,
        ] {
            let parsed: TrafficPriority = p.as_str().parse().unwrap();
            assert_eq!(parsed, p);
        }
    }

    #[test]
    fn parse_role_round_trip() {
        for &r in &[BufferRole::EncKeys, BufferRole::DecKeys] {
            assert_eq!(r.as_str().parse::<BufferRole>().unwrap(), r);
        }
    }

    #[test]
    fn parse_invalid_priority_errors() {
        assert!("not_a_class".parse::<TrafficPriority>().is_err());
    }

    #[test]
    fn registry_default_is_priority() {
        let r = BufferPriorityRegistry::new();
        assert_eq!(
            r.get("dA", "dB", BufferRole::EncKeys),
            TrafficPriority::Priority,
        );
    }

    #[test]
    fn registry_set_and_get() {
        let r = BufferPriorityRegistry::new();
        r.set("dA", "dB", BufferRole::EncKeys, TrafficPriority::BestEffort);
        assert_eq!(
            r.get("dA", "dB", BufferRole::EncKeys),
            TrafficPriority::BestEffort,
        );
        // Other (dkms, peer, role) tuples remain at default.
        assert_eq!(
            r.get("dA", "dB", BufferRole::DecKeys),
            TrafficPriority::Priority,
        );
        assert_eq!(
            r.get("dB", "dA", BufferRole::EncKeys),
            TrafficPriority::Priority,
        );
    }

    #[test]
    fn registry_overwrites_returning_previous() {
        let r = BufferPriorityRegistry::new();
        assert!(r
            .set("dA", "dB", BufferRole::EncKeys, TrafficPriority::Relax)
            .is_none());
        let prev = r.set("dA", "dB", BufferRole::EncKeys, TrafficPriority::Quickly);
        assert_eq!(prev, Some(TrafficPriority::Relax));
    }
}
