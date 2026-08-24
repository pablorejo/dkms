//! Data types shared between the MCMCF-λ solver and the rest of the
//! SDN (HTTP endpoints, forwarding push loop, downstream consumers).
//!
//! ## Phase 6 housekeeping
//!
//! The original `McfSolver` lived here — a strict-priority +
//! round-robin water-filling solver ported from the Python codebase.
//! It was retired in phase 3 once the MCMCF-λ LP took over, and
//! deleted in phase 6 once its QoS-class machinery (`priority.rs`)
//! went with it. What remains is the wire-level types that the LP
//! produces and that the rest of the SDN consumes:
//!
//! - [`flow_id`] — canonical `"src->dst"` string used as a map key.
//! - [`BufferRole`] — `EncKeys` / `DecKeys` enum for the per-buffer
//!   rate view in [`McfSnapshot::rates_by_dkms`].
//! - [`WcmpNextHop`] — one entry of a weighted forwarding table,
//!   serialised verbatim into the QKC's `POST /forwarding-table`
//!   body.
//! - [`McfSnapshot`] — the published view of "what the SDN wants
//!   every DKMS / QKC to do right now". Built by
//!   [`crate::mcmcf::McmcfSolution::into_mcf_snapshot`].

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use common::security::KeyGrade;
use serde::Serialize;

/// Build the canonical id used to key a flow `(src_dkms → dst_dkms)`.
/// Stable serialisation so two snapshots over identical inputs hash
/// the same.
pub fn flow_id(src_dkms: &str, dst_dkms: &str) -> String {
    format!("{src_dkms}->{dst_dkms}")
}

/// Which side of a directional commodity a buffer represents. Each
/// DKMS sees two roles for every peer:
///
/// * `EncKeys`: the buffer the DKMS *fills* when it acts as the
///   source of a commodity — keys it will hand to a SAE for
///   encryption.
/// * `DecKeys`: the mirror buffer the *destination* DKMS holds for
///   the same commodity — keys it uses to decrypt material handed
///   back by a SAE.
///
/// The MCMCF-λ commodity is single-directional (one `r_k` per
/// ordered pair), but the per-DKMS view in
/// [`McfSnapshot::rates_by_dkms`] surfaces both perspectives so the
/// DKMS' `/rate` poll can drive both buffers from the same snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub enum BufferRole {
    EncKeys,
    DecKeys,
}

impl BufferRole {
    pub fn as_str(&self) -> &'static str {
        match self {
            BufferRole::EncKeys => "enc_keys",
            BufferRole::DecKeys => "dec_keys",
        }
    }
}

impl fmt::Display for BufferRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BufferRole {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "enc_keys" => Ok(BufferRole::EncKeys),
            "dec_keys" => Ok(BufferRole::DecKeys),
            _ => Err(format!("unknown buffer role: {s}")),
        }
    }
}

/// One weighted next-hop entry in a WCMP forwarding table. Mirrors
/// the wire shape consumed by the QKC's `POST /forwarding-table`
/// endpoint (`{"qkc_id": N, "weight": W}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct WcmpNextHop {
    pub qkc_id: u32,
    pub weight: u32,
}

/// Published view of the SDN's current allocation: per-flow rates,
/// per-DKMS buffer rates, and the WCMP forwarding tables that go to
/// the QKCs.
///
/// Built by [`crate::mcmcf::McmcfSolution::into_mcf_snapshot`] and
/// stored behind an `ArcSwap` in [`crate::service::SdnService`].
#[derive(Debug, Default, Clone)]
pub struct McfSnapshot {
    /// `flow_id → r_f` (keys/s). Includes flows with `r_f = 0` so a
    /// DKMS polling `/rate` can distinguish "known commodity, no
    /// rate" from "unknown".
    pub rates: HashMap<String, f64>,

    /// `qkc_id → dst_qkc → Vec<WcmpNextHop>` — derivada de la topología y
    /// las capacidades ([`crate::mcmcf::wcmp_from_topology`]), **no** de los
    /// flujos del LP: la ruta debe existir siempre y cambiar al ritmo de la
    /// topología, no heredar los casos degenerados del reparto de tasas. El
    /// push la POSTea a `/forwarding-table` de cada QKC.
    ///
    /// Empty `(qkc, dst)` pairs fall back to topology
    /// `shortest_path_qkc` in the push loop.
    pub wcmp: HashMap<String, HashMap<String, Vec<WcmpNextHop>>>,

    /// `dkms_id → (peer_dkms, role) → r_f`. For a flow `A → B` with
    /// rate `r`:
    ///
    /// * `rates_by_dkms[A][(B, Enc)] = r` — what A pushes to B.
    /// * `rates_by_dkms[B][(A, Dec)] = r` — what B receives from A.
    ///
    /// Flows `A → B` and `B → A` are independent and may differ.
    ///
    /// **Aggregate over grades** — when a pair carries both a QKD-grade and a
    /// PQC-grade commodity this is their sum. Per-grade rates live in
    /// [`Self::rates_by_dkms_grade`].
    pub rates_by_dkms: HashMap<String, HashMap<(String, BufferRole), f64>>,

    /// Per-**grade** view of [`Self::rates_by_dkms`]:
    /// `dkms_id → (peer_dkms, role, grade) → r`. The DKMS uses this to fill its
    /// separate `(peer, grade)` buffers at the right rate per grade.
    pub rates_by_dkms_grade: HashMap<String, HashMap<(String, BufferRole, KeyGrade), f64>>,

    /// QKD-only WCMP tables: `qkc_id → dst_qkc → Vec<WcmpNextHop>`, built from
    /// the QKD-grade commodities' edge flows alone. Pushed to each QKC's
    /// `/forwarding-table` as `replace_qkd` so QKD-grade frames route strictly
    /// over QKD links. [`Self::wcmp`] remains the full-graph (any-grade) table.
    pub wcmp_qkd: HashMap<String, HashMap<String, Vec<WcmpNextHop>>>,
}

impl McfSnapshot {
    pub fn rate_for_flow(&self, src_dkms: &str, dst_dkms: &str) -> f64 {
        self.rates
            .get(&flow_id(src_dkms, dst_dkms))
            .copied()
            .unwrap_or(0.0)
    }

    /// Helper for the rate that should drive a particular buffer.
    pub fn rate_for_buffer(&self, dkms_id: &str, peer_dkms: &str, role: BufferRole) -> f64 {
        // The buffer rate is just the rate of the underlying flow.
        match role {
            BufferRole::EncKeys => self.rate_for_flow(dkms_id, peer_dkms),
            BufferRole::DecKeys => self.rate_for_flow(peer_dkms, dkms_id),
        }
    }
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_id_is_stable() {
        assert_eq!(flow_id("dA", "dB"), "dA->dB");
        assert_eq!(flow_id("dA", "dB"), flow_id("dA", "dB"));
    }

    #[test]
    fn buffer_role_serialises_and_parses() {
        assert_eq!(BufferRole::EncKeys.as_str(), "enc_keys");
        assert_eq!(BufferRole::DecKeys.as_str(), "dec_keys");
        assert_eq!(
            "enc_keys".parse::<BufferRole>().unwrap(),
            BufferRole::EncKeys
        );
        assert_eq!(
            "dec_keys".parse::<BufferRole>().unwrap(),
            BufferRole::DecKeys
        );
        assert!("priority".parse::<BufferRole>().is_err());
    }

    #[test]
    fn rate_for_buffer_resolves_encdec_views() {
        let mut snap = McfSnapshot::default();
        snap.rates.insert(flow_id("dA", "dB"), 100.0);
        snap.rates.insert(flow_id("dB", "dA"), 50.0);
        assert_eq!(snap.rate_for_buffer("dA", "dB", BufferRole::EncKeys), 100.0);
        assert_eq!(snap.rate_for_buffer("dB", "dA", BufferRole::DecKeys), 100.0);
        assert_eq!(snap.rate_for_buffer("dB", "dA", BufferRole::EncKeys), 50.0);
        assert_eq!(snap.rate_for_buffer("dA", "dB", BufferRole::DecKeys), 50.0);
    }

    #[test]
    fn rate_for_unknown_flow_is_zero() {
        let snap = McfSnapshot::default();
        assert_eq!(snap.rate_for_flow("dX", "dY"), 0.0);
        assert_eq!(snap.rate_for_buffer("dX", "dY", BufferRole::EncKeys), 0.0);
    }
}
