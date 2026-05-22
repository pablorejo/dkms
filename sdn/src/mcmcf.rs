//! Maximum Concurrent Multi-Commodity Flow with demand scaling — the
//! linear program described in `docs/mcmcf-lambda.tex`.
//!
//! ## Variables and constraints
//!
//! For each commodity `k ∈ K` and each directed arc `(i, j) ∈ A` we
//! introduce `x^k_{ij} ≥ 0`. The single scalar `λ ≥ 0` is the global
//! scale factor: every commodity sees its delivered rate
//!
//! ```text
//!     r_k = δ_k + λ · R_k         (paper eq. 1)
//! ```
//!
//! where `δ_k` is the SAE drain rate at the source DKMS and
//! `R_k = B_k − L_k` is the buffer space still available. The LP is
//!
//! ```text
//!     max λ
//!     s.t.  Σ_j x^k_{ij} − Σ_j x^k_{ji} = b^k_i              ∀ k, i ∈ V   (flow)
//!           Σ_k (x^k_{ij} + x^k_{ji})  ≤ u_{ij}              ∀ {i,j} ∈ E (capacity)
//!           x^k_{ij} ≥ 0,  λ ≥ 0
//!     with  b^k_i =  (δ_k + λ R_k)  if i = src_k
//!                 = -(δ_k + λ R_k)  if i = dst_k
//!                 =  0              otherwise.
//! ```
//!
//! Because `r_k` is the same `λ` for every commodity, all buffers
//! reach `B_k` at the same time `T* = 1/λ*` — that's Proposition 4.1
//! of the paper.
//!
//! ## Single-path projection (phase 3)
//!
//! The LP returns an edge-flow assignment that may split a commodity
//! across multiple paths. Phase 3 keeps the existing QKC forwarding
//! table shape (single next-hop per destination) so we project to a
//! single path per commodity by reading topology shortest paths in
//! `into_mcf_snapshot()`. The full multi-path decomposition lives in
//! phase 4.
//!
//! ## Bootstrap behaviour
//!
//! At SDN boot the [`crate::demand::DemandRegistry`] is empty.
//! Without synthetic entries the LP would return `λ = 0` for every
//! pair, and DKMSs polling `/rate` would see zero. To keep the
//! system useful before the first `POST /demand` lands, we
//! synthesise a commodity for **every** ordered DKMS pair the
//! topology can connect, defaulting to `(L_k = 0, B_k =
//! DEFAULT_BUFFER_CAPACITY, δ_k = 0)`. As real reports arrive they
//! override these defaults entry-by-entry.

use std::collections::{HashMap, HashSet};

use good_lp::{variable, Expression, ProblemVariables, Solution, SolverModel, Variable};
use tracing::{debug, warn};

use crate::{
    demand::{CommodityDemand, DemandRegistry},
    mcf::{flow_id, BufferRole, McfSnapshot, WcmpNextHop},
    topology::Topology,
};

/// Buffer capacity (keys) assumed when no DKMS has yet reported for
/// a commodity. Matches the DKMS default `BufferCfg.capacity_per_peer
/// = 4096` so the synthesized bootstrap value tracks reality
/// in-cluster.
pub const DEFAULT_BUFFER_CAPACITY: f64 = 4096.0;

/// Inputs the solver consumes: the QKC topology projected onto a
/// shared-capacity undirected graph plus the list of commodities to
/// schedule.
#[derive(Debug, Default, Clone)]
pub struct McmcfInputs {
    /// One entry per commodity. Order is deterministic — sorted by
    /// `(src_dkms, dst_dkms)` — so two solves with the same inputs
    /// produce the same edge-flow vector.
    pub commodities: Vec<CommodityDemand>,

    /// `(qkc_a, qkc_b) -> u_ij`, lexicographically ordered (a < b)
    /// so undirected edges aren't double-counted.
    pub edge_capacity: HashMap<(String, String), f64>,

    /// `dkms_id -> qkc_id` for every DKMS in the topology that has
    /// a resolvable QKC anchor.
    pub dkms_to_qkc: HashMap<String, String>,
}

impl McmcfInputs {
    /// Build the solver inputs from a topology snapshot and the
    /// current demand registry. Commodities for which no DKMS has
    /// reported yet are synthesised with `(0, DEFAULT_BUFFER_CAPACITY,
    /// 0)` so the LP still has work to do at SDN boot — see the
    /// module-level "Bootstrap behaviour" note.
    pub fn build(topology: &Topology, registry: &DemandRegistry) -> Self {
        // dkms_to_qkc map (skip DKMSs whose ORR or QKC has been
        // deleted — they can't be commodity endpoints anyway).
        let mut dkms_to_qkc: HashMap<String, String> = HashMap::new();
        for dkms_id in topology.dkms.keys() {
            if let Some(qkc) = topology.qkc_of_dkms(dkms_id) {
                dkms_to_qkc.insert(dkms_id.clone(), qkc.to_string());
            }
        }

        // Edge capacity table (canonical key form a < b).
        let mut edge_capacity: HashMap<(String, String), f64> = HashMap::new();
        for ((a, b), meta) in &topology.edges {
            let cap = meta.quditto_capacity_keys_per_second();
            if cap > 0.0 {
                edge_capacity.insert((a.clone(), b.clone()), cap);
            }
        }

        // Commodity set: every ordered pair (src, dst) of DKMSs in
        // distinct QKCs gets a slot. Override with registry entries
        // where they exist.
        let mut commodities: Vec<CommodityDemand> = Vec::new();
        let dkms_ids: Vec<&String> = dkms_to_qkc.keys().collect();
        for src in &dkms_ids {
            for dst in &dkms_ids {
                if src == dst {
                    continue;
                }
                let src_qkc = &dkms_to_qkc[*src];
                let dst_qkc = &dkms_to_qkc[*dst];
                if src_qkc == dst_qkc {
                    // Same QKC anchor — no network flow needed
                    // between them at all.
                    continue;
                }
                let entry =
                    registry
                        .get(src.as_str(), dst.as_str())
                        .unwrap_or_else(|| CommodityDemand {
                            src_dkms: (*src).clone(),
                            dst_dkms: (*dst).clone(),
                            level: 0.0,
                            capacity: DEFAULT_BUFFER_CAPACITY,
                            drain_rate: 0.0,
                            timestamp_ms: 0,
                        });
                commodities.push(entry);
            }
        }
        commodities.sort_by(|a, b| {
            (a.src_dkms.as_str(), a.dst_dkms.as_str())
                .cmp(&(b.src_dkms.as_str(), b.dst_dkms.as_str()))
        });

        Self {
            commodities,
            edge_capacity,
            dkms_to_qkc,
        }
    }
}

/// One non-zero edge-flow assignment: how much of `flow_id` crosses
/// the directed arc `(src_qkc → dst_qkc)`.
#[derive(Debug, Clone, PartialEq)]
pub struct EdgeFlow {
    pub flow_id: String,
    pub src_qkc: String,
    pub dst_qkc: String,
    pub flow: f64,
}

/// What the LP returns: the global scale factor, the edge-flow
/// support, and a flat per-flow rate view.
#[derive(Debug, Default, Clone)]
pub struct McmcfSolution {
    /// `λ*` from the paper. `r_k = δ_k + λ* · R_k` for every k.
    /// `0.0` means the LP was infeasible or had no commodities to
    /// schedule.
    pub lambda: f64,

    /// Non-zero edge flows. Sparse: arcs with `x^k_{ij} ≤ ε` are
    /// omitted (ε = `FLOW_EPSILON`).
    pub edge_flows: Vec<EdgeFlow>,

    /// `flow_id → r_k`. Includes flows with `r_k = 0` (so callers
    /// can distinguish "known commodity, no rate" from "unknown").
    pub rates: HashMap<String, f64>,
}

/// Flows smaller than this in absolute value get truncated to 0.
/// Both for the edge-flow support and for the rate view. Picks up
/// solver numerical noise without losing real allocations — the
/// smallest meaningful rate in practice is `δ_k > 0`, which is at
/// least 1 key/s for any active SAE.
const FLOW_EPSILON: f64 = 1e-6;

/// Horizon (seconds) over which a single LP solve is considered
/// authoritative. The LP delivers `r_k = δ_k + λ·R_k + η_k` for the
/// commodity; capping `η_k ≤ R_k / T_REPLAN` ensures the buffer
/// can't overflow before the next solve replaces these rates with
/// fresh ones. Matches the SDN's default `mcf_period_ms = 5000`.
const T_REPLAN_SECONDS: f64 = 5.0;

/// Penalty multiplier on `Σ σ_k` in phase-1 LP. With `λ` in the
/// range `[0, 10]` and `σ_k` in the range `[0, max_drain]` (≈ 1e3),
/// a coefficient of 1e3 makes the LP minimise σ absolutely before
/// touching λ — the strict lex order between "deliver δ" and
/// "fill at λ" required by paper §6's relaxation.
const LAMBDA_SLACK_PENALTY: f64 = 1e3;

/// Lower-bound slack for phase 2's `λ ≥ λ*` constraint. microlp's
/// simplex has solve-to-solve numerical drift; an exact equality
/// `λ == λ*` would risk infeasibility when `λ*` came back as
/// `λ* − ε_machine`. Five orders of magnitude below the smallest
/// interesting λ (≈ 1e-3) is harmless and avoids most
/// false-infeasibility cases on small LPs. Larger LPs (e.g.
/// 380-commodity ER topologies) still hit false-infeasibility at
/// `~70 %` of solves with this value — the fallback (η = 0) keeps
/// the system functionally correct but the lex refinement
/// effectively disengages. Improving this is a known TODO; see
/// `project_mcmcf_er20_smoke.md`.
const LAMBDA_FIX_SLACK: f64 = 1e-8;

impl McmcfSolution {
    /// Adapt the solver output to the legacy [`McfSnapshot`] shape so
    /// the rest of the SDN (the `/rate` endpoint, the forwarding push
    /// loop, the `rates_by_dkms` view consumed by DKMSs) keeps working.
    ///
    /// Phase 4: populates `wcmp` from the LP's edge-flow assignment so
    /// the forwarding push pushes WCMP tables instead of single-path
    /// tables. Missing pairs (LP didn't allocate any flow between
    /// them) are left absent — the push loop falls back to topology
    /// shortest-path for those.
    pub fn into_mcf_snapshot(self, topology: &Topology) -> McfSnapshot {
        let mut snap = McfSnapshot::default();

        // Rates: flat + ENC/DEC mirror.
        for (flow_id_str, rate) in self.rates.iter() {
            let Some((src, dst)) = flow_id_str.split_once("->") else {
                warn!(flow_id = %flow_id_str, "malformed flow_id in solution");
                continue;
            };
            snap.rates.insert(flow_id_str.clone(), *rate);
            snap.rates_by_dkms
                .entry(src.to_string())
                .or_default()
                .insert((dst.to_string(), BufferRole::EncKeys), *rate);
            snap.rates_by_dkms
                .entry(dst.to_string())
                .or_default()
                .insert((src.to_string(), BufferRole::DecKeys), *rate);
        }

        // WCMP: aggregate edge-flows per `(transit_qkc, commodity_dst_qkc)`.
        snap.wcmp = wcmp_from_edge_flows(&self.edge_flows, topology);
        snap
    }
}

/// Granularity of WCMP weight quantisation. The LP yields edge flows
/// in keys/s (positive floats). We multiply by this scale and round
/// to integers so 1-key/s differences register as 100-unit weight
/// deltas — enough resolution for `[3, 1]`-style ratios while staying
/// inside `u32`.
const WCMP_WEIGHT_SCALE: f64 = 100.0;
/// Floor weight after quantisation. A `weight = 0` entry would be
/// stripped by the QKC's `ForwardingTable::replace` sanitiser, which
/// would silently break the path — keep at least 1 so the route
/// stays usable.
const WCMP_MIN_WEIGHT: u32 = 1;

/// Build per-QKC WCMP tables from a set of edge-flow assignments.
///
/// For each `(transit_qkc q, commodity destination d)` pair, sum the
/// LP flow leaving `q` toward each neighbour, **but only for
/// commodities whose own dst_qkc equals d**. The aggregated flows
/// per neighbour become the WCMP weights at `q` for traffic going
/// to `d`.
///
/// Returns the map in the [`McfSnapshot::wcmp`] shape:
/// `qkc_id → dst_qkc → Vec<WcmpNextHop>`. Pairs not represented in
/// the input are omitted — the push loop fills the gaps with
/// shortest-path entries.
pub fn wcmp_from_edge_flows(
    edge_flows: &[EdgeFlow],
    topology: &Topology,
) -> HashMap<String, HashMap<String, Vec<WcmpNextHop>>> {
    // (transit_qkc, commodity_dst_qkc) → (neighbour → cumulative flow)
    let mut buckets: HashMap<(String, String), HashMap<String, f64>> = HashMap::new();
    for ef in edge_flows {
        let Some((_src_dkms, dst_dkms)) = ef.flow_id.split_once("->") else {
            continue;
        };
        let Some(dst_qkc) = topology.qkc_of_dkms(dst_dkms) else {
            continue;
        };
        // ef.src_qkc is the QKC the flow leaves; ef.dst_qkc is the
        // next-hop neighbour — different from the *commodity's*
        // destination QKC (which is what we group by).
        *buckets
            .entry((ef.src_qkc.clone(), dst_qkc.to_string()))
            .or_default()
            .entry(ef.dst_qkc.clone())
            .or_insert(0.0) += ef.flow;
    }

    // Quantise into `WcmpNextHop` entries.
    let mut out: HashMap<String, HashMap<String, Vec<WcmpNextHop>>> = HashMap::new();
    for ((transit, dst), neighbours) in buckets {
        let mut hops: Vec<WcmpNextHop> = neighbours
            .into_iter()
            .filter(|(_, flow)| *flow > FLOW_EPSILON)
            .filter_map(|(nh_id, flow)| {
                let qkc_id: u32 = nh_id.parse().ok()?;
                let weight =
                    ((flow * WCMP_WEIGHT_SCALE).round() as i64).max(WCMP_MIN_WEIGHT as i64) as u32;
                Some(WcmpNextHop { qkc_id, weight })
            })
            .collect();
        if hops.is_empty() {
            continue;
        }
        // Deterministic order so two snapshots with the same data
        // serialise identically (helps test assertions and diff
        // logs).
        hops.sort_by_key(|h| h.qkc_id);
        out.entry(transit).or_default().insert(dst, hops);
    }
    out
}

/// MCMCF-λ solver handle. Stateless today — the LP is built fresh on
/// every solve. Held by value so callers don't need to lock it.
#[derive(Debug, Default, Clone, Copy)]
pub struct McmcfSolver;

impl McmcfSolver {
    pub fn new() -> Self {
        Self
    }

    /// Solve the MCMCF-λ instance described by `inputs`. Returns the
    /// zero solution when the LP is infeasible or has no work
    /// (empty topology, no commodities, all flows trivial).
    pub fn solve(&self, inputs: &McmcfInputs) -> McmcfSolution {
        // Filter commodities to those that can actually carry flow:
        // both endpoints must map to a QKC that exists in the graph.
        // Same-QKC pairs are already dropped by `build()`.
        let active: Vec<&CommodityDemand> = inputs
            .commodities
            .iter()
            .filter(|c| {
                inputs.dkms_to_qkc.contains_key(&c.src_dkms)
                    && inputs.dkms_to_qkc.contains_key(&c.dst_dkms)
            })
            .collect();
        if active.is_empty() || inputs.edge_capacity.is_empty() {
            return McmcfSolution::default();
        }

        // Short-circuit when **every** commodity has `R_k = 0`. In
        // that case `λ` has a zero coefficient in every conservation
        // constraint, the objective `max λ` is unbounded, and microlp
        // bails with `Unbounded`. Mathematically the right answer is
        // `λ = 0` with `r_k = δ_k` — the buffers are full, we just
        // compensate drainage (or do nothing when δ_k = 0 too).
        //
        // We still populate `rates` with one entry per commodity so
        // DKMSs polling `/rate` see explicit zeros instead of an
        // empty `peers` map (which the legacy code paths would
        // interpret as "DKMS unknown").
        if !active.iter().any(|c| c.remaining() > 0.0) {
            let mut rates: HashMap<String, f64> = HashMap::with_capacity(active.len());
            for c in &active {
                rates.insert(flow_id(&c.src_dkms, &c.dst_dkms), c.drain_rate);
            }
            return McmcfSolution {
                lambda: 0.0,
                edge_flows: Vec::new(),
                rates,
            };
        }

        // ----- arc list & node set ---------------------------------------
        // For every undirected edge {a,b} we synthesise two directed
        // arcs (a,b) and (b,a) that share the underlying capacity
        // via the constraint Σ_k (x^k_{ij} + x^k_{ji}) ≤ u_ij.
        let mut arcs: Vec<(String, String)> = Vec::with_capacity(inputs.edge_capacity.len() * 2);
        let mut arc_idx: HashMap<(String, String), usize> = HashMap::new();
        let mut undirected: Vec<((String, String), f64)> =
            Vec::with_capacity(inputs.edge_capacity.len());
        for ((a, b), cap) in &inputs.edge_capacity {
            let key = (a.clone(), b.clone());
            undirected.push((key.clone(), *cap));
            arc_idx.insert((a.clone(), b.clone()), arcs.len());
            arcs.push((a.clone(), b.clone()));
            arc_idx.insert((b.clone(), a.clone()), arcs.len());
            arcs.push((b.clone(), a.clone()));
        }
        let node_set: HashSet<String> = arcs
            .iter()
            .flat_map(|(a, b)| [a.clone(), b.clone()])
            .collect();

        // The LP is solved in TWO PHASES to implement the lex
        // refinement of paper §4 without falling into the ε-method
        // trap. The naive `max λ + ε·Σ η_k` objective fails when
        // `λ` is small relative to `η_max`: at ε=1e-3 and λ≈0.01,
        // the LP gladly drops λ to zero and pumps the budget into
        // η. The fix is to make the precedence exact:
        //
        //   Phase 1: max λ                  (no η in LP) → λ*
        //   Phase 2: max Σ η_k    s.t. λ ≥ λ* − slack    → η_k*
        //
        // Both phases share the same arc/node setup; only the
        // variable set and objective change.

        // ----- Phase 1: solve max λ − M·Σ σ_k ---------------------------
        let (lambda_star, sigmas) =
            match self.solve_lambda(&active, &arcs, &arc_idx, &node_set, &undirected, inputs) {
                Some(v) => v,
                None => return zero_rate_fallback(&active),
            };

        // ----- Phase 2: solve max Σ η_k subject to λ ≥ λ* − slack ---------
        // Lex refinement is opt-in via env var because microlp's
        // simplex hits false-infeasibility on large LPs (e.g. ≥ 380
        // commodities). When that happens the fallback (η = 0) is
        // chosen for that solve only — but if the solver oscillates
        // between success and false-infeasibility across recomputes,
        // the residual rates spike and dip in a way that looks like
        // noise to downstream consumers. Setting
        // `SDN_DISABLE_LEX_REFINEMENT=1` forces phase 1 behaviour
        // (no η, exact `r_k = δ_k + λ* · R_k`) for those cases.
        let lex_disabled = std::env::var("SDN_DISABLE_LEX_REFINEMENT")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let phase2 = if lex_disabled {
            Phase2Output {
                eta_values: vec![0.0; active.len()],
                edge_flows: Vec::new(),
            }
        } else {
            self.solve_eta(
                lambda_star,
                &active,
                &arcs,
                &arc_idx,
                &node_set,
                &undirected,
                inputs,
            )
        };

        // ----- assemble final solution ------------------------------------
        let mut rates: HashMap<String, f64> = HashMap::with_capacity(active.len());
        let mut total_eta = 0.0;
        let mut total_sigma = 0.0;
        for (k_idx, c) in active.iter().enumerate() {
            let eta_k = phase2.eta_values.get(k_idx).copied().unwrap_or(0.0);
            let sigma_k = sigmas.get(k_idx).copied().unwrap_or(0.0);
            total_eta += eta_k;
            total_sigma += sigma_k;
            // Effective delivered rate (paper §4 + §6 slack):
            //     r_k = (δ_k − σ_k) + λ·R_k + η_k
            // The σ slack absorbs unmet drain — `r_k` is what the
            // network can actually deliver, which is what the DKMS'
            // SaeBufferBuckets uses as `link_capacity` for refill.
            let delivered_drain = (c.drain_rate - sigma_k).max(0.0);
            let r_k = delivered_drain + lambda_star * c.remaining() + eta_k;
            let fid = flow_id(&c.src_dkms, &c.dst_dkms);
            let r_k_clean = if r_k.abs() < FLOW_EPSILON { 0.0 } else { r_k };
            rates.insert(fid, r_k_clean);
        }

        debug!(
            n_commodities = active.len(),
            n_arcs = arcs.len(),
            lambda = lambda_star,
            total_eta,
            total_sigma,
            n_edge_flows = phase2.edge_flows.len(),
            "MCMCF-λ solved (two-phase)"
        );

        McmcfSolution {
            lambda: lambda_star,
            edge_flows: phase2.edge_flows,
            rates,
        }
    }

    /// Phase 1 of the lex-refined solve: `max λ − M·Σ σ_k` with per-
    /// commodity *slack variables* `σ_k ∈ [0, δ_k]` that absorb any
    /// `δ_k` the network cannot route (paper §6 — "Factibilidad").
    ///
    /// Without slack, the LP becomes Infeasible whenever the
    /// aggregate SAE drain exceeds the min-cut capacity (typical of
    /// any sustained over-rate scenario), and the SDN falls back to
    /// `r_k = 0` for every commodity → buffers empty → cascade fail.
    /// With slack, the LP always has a solution: it routes as much
    /// as the network can, books the rest as unmet demand σ, and
    /// the SAEs see graceful 429s via the DKMS-side bucket (refill
    /// at the *deliverable* rate `δ_k − σ_k + λ·R_k`).
    ///
    /// Returns `(λ*, σ_k* per commodity)`. The caller computes the
    /// effective delivered rate as
    ///     `r_k = (δ_k − σ_k) + λ* · R_k`.
    ///
    /// Penalty `LAMBDA_SLACK_PENALTY` is chosen so even one unit of
    /// `σ` dominates the entire feasible range of λ — the LP minimises
    /// unmet demand absolutely first, then maximises λ.
    fn solve_lambda(
        &self,
        active: &[&CommodityDemand],
        arcs: &[(String, String)],
        arc_idx: &HashMap<(String, String), usize>,
        node_set: &HashSet<String>,
        undirected: &[((String, String), f64)],
        inputs: &McmcfInputs,
    ) -> Option<(f64, Vec<f64>)> {
        let mut vars = ProblemVariables::new();
        let lambda = vars.add(variable().min(0.0));
        let x: Vec<Vec<Variable>> = active
            .iter()
            .map(|_| {
                (0..arcs.len())
                    .map(|_| vars.add(variable().min(0.0)))
                    .collect()
            })
            .collect();
        // σ_k ∈ [0, δ_k] — "unmet drain" per commodity. Active only
        // for commodities with δ_k > 0 (a fill-only commodity can't
        // have unmet drain).
        //
        // `SDN_DISABLE_SLACK_VARS=1` forces σ_k = 0 (upper bound 0)
        // for all commodities. Used to investigate if σ_k introduces
        // numerical drift between phase-1 and phase-2 LPs that causes
        // phase-2 to report false-infeasible — when phase-1 uses
        // σ_k > 0, phase-2's stricter formulation (no slack) cannot
        // reproduce the phase-1 routing exactly.
        let slack_disabled = std::env::var("SDN_DISABLE_SLACK_VARS")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let sigma: Vec<Variable> = active
            .iter()
            .map(|c| {
                let upper = if slack_disabled {
                    0.0
                } else {
                    c.drain_rate.max(0.0)
                };
                vars.add(variable().min(0.0).max(upper))
            })
            .collect();
        // Build objective: λ − M·Σ σ_k.
        let mut obj = Expression::with_capacity(1 + sigma.len());
        obj += lambda;
        for s in &sigma {
            obj += LAMBDA_SLACK_PENALTY * -1.0 * *s;
        }
        let mut problem = vars.maximise(obj).using(good_lp::default_solver);
        for (k_idx, c) in active.iter().enumerate() {
            let src = inputs.dkms_to_qkc.get(c.src_dkms.as_str()).unwrap();
            let dst = inputs.dkms_to_qkc.get(c.dst_dkms.as_str()).unwrap();
            let r_k = c.remaining();
            let delta_k = c.drain_rate;
            let sigma_k = sigma[k_idx];
            for node in node_set {
                let mut expr = Expression::with_capacity(arcs.len());
                for (a_idx, (a, b)) in arcs.iter().enumerate() {
                    if a == node {
                        expr += x[k_idx][a_idx];
                    }
                    if b == node {
                        expr -= x[k_idx][a_idx];
                    }
                }
                // Conservation with σ slack at source/sink:
                //   src: out - in - λ·R_k = δ_k - σ_k
                //   dst: out - in + λ·R_k = -(δ_k - σ_k)
                let c_node = if node == src {
                    (expr - lambda * r_k + sigma_k).eq(delta_k)
                } else if node == dst {
                    (expr + lambda * r_k - sigma_k).eq(-delta_k)
                } else {
                    expr.eq(0.0)
                };
                problem = problem.with(c_node);
            }
        }
        for ((a, b), cap) in undirected {
            let ij = arc_idx[&(a.clone(), b.clone())];
            let ji = arc_idx[&(b.clone(), a.clone())];
            let mut expr = Expression::with_capacity(2 * active.len());
            for row in x.iter().take(active.len()) {
                expr += row[ij];
                expr += row[ji];
            }
            problem = problem.with(expr.leq(*cap));
        }
        match problem.solve() {
            Ok(sol) => {
                let lambda_v = sol.value(lambda).max(0.0);
                let sigmas: Vec<f64> = sigma.iter().map(|s| sol.value(*s).max(0.0)).collect();
                Some((lambda_v, sigmas))
            }
            Err(e) => {
                warn!(error = %e, "MCMCF-λ phase-1 LP failed");
                None
            }
        }
    }

    /// Phase 2: with `λ` pinned to `lambda_star` (modulo a small
    /// slack), maximise `Σ η_k`. Returns the per-commodity `η_k`
    /// values **and** the edge-flow support of this solve — the
    /// final routing is the phase-2 LP's, not phase 1's, since the
    /// extra η flow needs to live somewhere on the graph.
    fn solve_eta(
        &self,
        lambda_star: f64,
        active: &[&CommodityDemand],
        arcs: &[(String, String)],
        arc_idx: &HashMap<(String, String), usize>,
        node_set: &HashSet<String>,
        undirected: &[((String, String), f64)],
        inputs: &McmcfInputs,
    ) -> Phase2Output {
        let mut vars = ProblemVariables::new();
        let lambda = vars.add(variable().min(0.0));
        let x: Vec<Vec<Variable>> = active
            .iter()
            .map(|_| {
                (0..arcs.len())
                    .map(|_| vars.add(variable().min(0.0)))
                    .collect()
            })
            .collect();
        // Lex refinement (paper §4): one `η_k ≥ 0` per commodity.
        // Upper bound `R_k / T_REPLAN_SECONDS` prevents the
        // residual rate from overfilling the buffer within one
        // re-plan horizon. Commodities with `R_k = 0` get
        // `η_k = 0` because the upper bound collapses to 0.
        let eta: Vec<Variable> = active
            .iter()
            .map(|c| {
                let upper = (c.remaining() / T_REPLAN_SECONDS).max(0.0);
                vars.add(variable().min(0.0).max(upper))
            })
            .collect();
        // Objective: maximise sum of η_k.
        let mut obj = Expression::with_capacity(eta.len());
        for e in &eta {
            obj += *e;
        }
        let mut problem = vars.maximise(obj).using(good_lp::default_solver);
        // Pin λ to phase-1 optimum (with small slack to absorb
        // microlp's solve-to-solve numerical drift).
        problem = problem.with((lambda - lambda_star).geq(-LAMBDA_FIX_SLACK));
        // Conservation with η_k present at source/sink.
        for (k_idx, c) in active.iter().enumerate() {
            let src = inputs.dkms_to_qkc.get(c.src_dkms.as_str()).unwrap();
            let dst = inputs.dkms_to_qkc.get(c.dst_dkms.as_str()).unwrap();
            let r_k = c.remaining();
            let delta_k = c.drain_rate;
            let eta_k = eta[k_idx];
            for node in node_set {
                let mut expr = Expression::with_capacity(arcs.len());
                for (a_idx, (a, b)) in arcs.iter().enumerate() {
                    if a == node {
                        expr += x[k_idx][a_idx];
                    }
                    if b == node {
                        expr -= x[k_idx][a_idx];
                    }
                }
                let c_node = if node == src {
                    (expr - lambda * r_k - eta_k).eq(delta_k)
                } else if node == dst {
                    (expr + lambda * r_k + eta_k).eq(-delta_k)
                } else {
                    expr.eq(0.0)
                };
                problem = problem.with(c_node);
            }
        }
        // Capacity (unchanged from phase 1).
        for ((a, b), cap) in undirected {
            let ij = arc_idx[&(a.clone(), b.clone())];
            let ji = arc_idx[&(b.clone(), a.clone())];
            let mut expr = Expression::with_capacity(2 * active.len());
            for row in x.iter().take(active.len()) {
                expr += row[ij];
                expr += row[ji];
            }
            problem = problem.with(expr.leq(*cap));
        }
        match problem.solve() {
            Ok(sol) => {
                let eta_values: Vec<f64> = eta.iter().map(|e| sol.value(*e).max(0.0)).collect();
                let mut edge_flows: Vec<EdgeFlow> = Vec::new();
                for (k_idx, c) in active.iter().enumerate() {
                    let fid = flow_id(&c.src_dkms, &c.dst_dkms);
                    for (a_idx, (a, b)) in arcs.iter().enumerate() {
                        let v = sol.value(x[k_idx][a_idx]);
                        if v > FLOW_EPSILON {
                            edge_flows.push(EdgeFlow {
                                flow_id: fid.clone(),
                                src_qkc: a.clone(),
                                dst_qkc: b.clone(),
                                flow: v,
                            });
                        }
                    }
                }
                Phase2Output {
                    eta_values,
                    edge_flows,
                }
            }
            Err(e) => {
                // Phase 2 LP should never fail when phase 1
                // succeeded (the feasible region only shrinks by a
                // single inequality), but if microlp slips
                // numerically we keep λ from phase 1 and zero η —
                // the result is the pre-fase-5 behaviour, no regression.
                warn!(error = %e, "MCMCF-λ phase-2 LP failed; falling back to η = 0");
                Phase2Output {
                    eta_values: vec![0.0; active.len()],
                    edge_flows: Vec::new(),
                }
            }
        }
    }
}

/// Result of the phase-2 LP: per-commodity `η_k` and the edge-flow
/// assignment that carries the augmented rate `r_k = δ_k + λ·R_k +
/// η_k`. The edge_flows here are the *final* routing — phase 1's
/// flow assignment is throwaway intermediate.
struct Phase2Output {
    eta_values: Vec<f64>,
    edge_flows: Vec<EdgeFlow>,
}

/// Build the safe fallback solution: `λ = 0`, `r_k = 0` per
/// commodity, no edge flows. Used when an LP solve fails — every
/// commodity still gets a `rates` entry so DKMSs polling `/rate`
/// don't see an empty `peers` map and conclude "DKMS unknown".
fn zero_rate_fallback(active: &[&CommodityDemand]) -> McmcfSolution {
    let mut rates: HashMap<String, f64> = HashMap::with_capacity(active.len());
    for c in active {
        rates.insert(flow_id(&c.src_dkms, &c.dst_dkms), 0.0);
    }
    McmcfSolution {
        lambda: 0.0,
        edge_flows: Vec::new(),
        rates,
    }
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Dkms, EdgeMeta, HostEndpoint, Orr, Qkc, Topology};

    fn host(id: i64) -> HostEndpoint {
        HostEndpoint {
            id,
            ip: format!("10.0.0.{id}"),
            port: 9000 + id as u16,
        }
    }

    fn add_dkms_pair(t: &mut Topology, da: &str, db: &str, qa: &str, qb: &str) {
        // Each side: QKC + ORR + DKMS chain.
        for q in [qa, qb] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap_or(0)),
                kme_host: None,
            });
        }
        t.upsert_orr(Orr {
            id: format!("o-{qa}"),
            host: host(10 + qa.parse::<i64>().unwrap_or(0)),
            qkc_id: qa.into(),
        });
        t.upsert_orr(Orr {
            id: format!("o-{qb}"),
            host: host(10 + qb.parse::<i64>().unwrap_or(0)),
            qkc_id: qb.into(),
        });
        t.upsert_dkms(Dkms {
            id: da.into(),
            host: host(20),
            tls_id: None,
            orr_id: format!("o-{qa}"),
        });
        t.upsert_dkms(Dkms {
            id: db.into(),
            host: host(21),
            tls_id: None,
            orr_id: format!("o-{qb}"),
        });
    }

    fn link(t: &mut Topology, a: &str, b: &str, r0: f64) {
        t.add_edge(
            a,
            b,
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: r0,
                alpha: 0.2,
                max_buffer_size: 10,
            },
        );
    }

    fn report(reg: &DemandRegistry, src: &str, dst: &str, level: f64, cap: f64, drain: f64) {
        use crate::demand::DemandReport;
        reg.ingest(DemandReport {
            dkms_id: src.into(),
            entries: vec![CommodityDemand {
                src_dkms: src.into(),
                dst_dkms: dst.into(),
                level,
                capacity: cap,
                drain_rate: drain,
                timestamp_ms: 1,
            }],
        });
    }

    /// Two DKMSs on the same QKC produce no commodity — both endpoints
    /// drain locally, no network flow involved.
    #[test]
    fn build_skips_same_qkc_pairs() {
        let mut t = Topology::default();
        t.upsert_qkc(Qkc {
            id: "1".into(),
            host: host(1),
            kme_host: None,
        });
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: host(11),
            qkc_id: "1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dA".into(),
            host: host(21),
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(22),
            tls_id: None,
            orr_id: "o1".into(),
        });
        let reg = DemandRegistry::new();
        let inputs = McmcfInputs::build(&t, &reg);
        assert_eq!(inputs.commodities.len(), 0);
    }

    /// Two DKMSs, distinct QKCs connected by one edge of cap 100. No
    /// drain. The LP must find the only path and set rates s.t. each
    /// direction gets capacity / 2 (since both fight for the same edge).
    #[test]
    fn solves_two_node_topology_shared_capacity() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        let inputs = McmcfInputs::build(&t, &reg);
        assert_eq!(inputs.commodities.len(), 2); // (A,B) and (B,A)
        let sol = McmcfSolver::new().solve(&inputs);
        // capacity 100 shared between two commodities → each gets 50.
        // r_k = δ_k + λ R_k. With δ=0 and R_k = DEFAULT_BUFFER_CAPACITY:
        //   r_k = λ * 4096 = 50  ⟹  λ = 50/4096 ≈ 0.0122
        let r_ab = sol.rates[&flow_id("dA", "dB")];
        let r_ba = sol.rates[&flow_id("dB", "dA")];
        assert!(
            (r_ab - 50.0).abs() < 1.0,
            "expected ≈50 each, got A→B={r_ab}, B→A={r_ba}"
        );
        assert!((r_ba - 50.0).abs() < 1.0);
        assert!((r_ab + r_ba - 100.0).abs() < 1e-3);
    }

    /// Drain compensates: a buffer with R_k = 0 still gets r_k = δ_k
    /// (rate exactly compensates drain, no fill).
    #[test]
    fn full_buffer_with_drain_gets_only_drain_rate() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        // dA→dB: buffer full (R_k = 0), drain 30 kps. dB→dA: empty + 0 drain.
        report(&reg, "dA", "dB", 4096.0, 4096.0, 30.0);
        report(&reg, "dB", "dA", 0.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        let r_ab = sol.rates[&flow_id("dA", "dB")];
        let r_ba = sol.rates[&flow_id("dB", "dA")];
        // r_ab = 30 + λ·0 = 30 (drain compensation only)
        // capacity remaining for B→A = 100 − 30 = 70 → r_ba = 70.
        // (B→A's R_k = 4096; with λ=70/4096 and r_ab also recomputes
        // as 30 + (70/4096)·0 = 30 ✓)
        assert!((r_ab - 30.0).abs() < 0.5, "r_ab should = δ_k, got {r_ab}");
        assert!((r_ba - 70.0).abs() < 0.5, "r_ba should ≈70, got {r_ba}");
    }

    /// Synchronisation invariant (Prop. 4.1): every buffer reaches B_k
    /// in the same time T* = 1/λ*. Verify directly from the solution.
    #[test]
    fn all_buffers_fill_simultaneously() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 200.0);
        let reg = DemandRegistry::new();
        // Asymmetric levels & drains — Prop. 4.1 should still hold.
        report(&reg, "dA", "dB", 1000.0, 4096.0, 20.0);
        report(&reg, "dB", "dA", 3000.0, 4096.0, 5.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        assert!(sol.lambda > 0.0);
        let t_star = 1.0 / sol.lambda;
        for c in McmcfInputs::build(&t, &reg).commodities.iter() {
            let r_k = sol.rates[&flow_id(&c.src_dkms, &c.dst_dkms)];
            let fill_time = c.remaining() / (r_k - c.drain_rate);
            assert!(
                (fill_time - t_star).abs() < 1e-3,
                "commodity {}->{}: fill_time={fill_time}, T*={t_star}",
                c.src_dkms,
                c.dst_dkms
            );
        }
    }

    /// Edge-flow conservation: at every non-endpoint node, the sum
    /// of incoming x^k equals the sum of outgoing x^k. Re-verifies
    /// the LP wired flow conservation correctly.
    #[test]
    fn edge_flow_conservation_at_intermediate_nodes() {
        // Linear path 1 — 2 — 3 with DKMSs at QKCs 1 and 3.
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "3");
        // Add the middle QKC.
        t.upsert_qkc(Qkc {
            id: "2".into(),
            host: host(2),
            kme_host: None,
        });
        link(&mut t, "1", "2", 100.0);
        link(&mut t, "2", "3", 100.0);
        let reg = DemandRegistry::new();
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 0.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        // For every commodity, check QKC 2 is balanced.
        for fid in [flow_id("dA", "dB"), flow_id("dB", "dA")] {
            let in_sum: f64 = sol
                .edge_flows
                .iter()
                .filter(|e| e.flow_id == fid && e.dst_qkc == "2")
                .map(|e| e.flow)
                .sum();
            let out_sum: f64 = sol
                .edge_flows
                .iter()
                .filter(|e| e.flow_id == fid && e.src_qkc == "2")
                .map(|e| e.flow)
                .sum();
            assert!(
                (in_sum - out_sum).abs() < 1e-3,
                "{fid}: in={in_sum}, out={out_sum}"
            );
        }
    }

    /// Empty inputs → zero solution. The solver must not panic on
    /// degenerate cases.
    #[test]
    fn empty_inputs_return_zero_solution() {
        let sol = McmcfSolver::new().solve(&McmcfInputs::default());
        assert_eq!(sol.lambda, 0.0);
        assert!(sol.edge_flows.is_empty());
        assert!(sol.rates.is_empty());
    }

    #[test]
    fn into_mcf_snapshot_populates_both_views() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let mut rates = HashMap::new();
        rates.insert(flow_id("dA", "dB"), 100.0);
        rates.insert(flow_id("dB", "dA"), 200.0);
        let sol = McmcfSolution {
            lambda: 0.5,
            edge_flows: vec![],
            rates,
        };
        let snap = sol.into_mcf_snapshot(&t);
        // Flat view.
        assert_eq!(snap.rates[&flow_id("dA", "dB")], 100.0);
        assert_eq!(snap.rates[&flow_id("dB", "dA")], 200.0);
        // ENC/DEC mirror view.
        assert_eq!(
            snap.rates_by_dkms["dA"][&("dB".to_string(), BufferRole::EncKeys)],
            100.0
        );
        assert_eq!(
            snap.rates_by_dkms["dB"][&("dA".to_string(), BufferRole::DecKeys)],
            100.0
        );
        assert_eq!(
            snap.rates_by_dkms["dB"][&("dA".to_string(), BufferRole::EncKeys)],
            200.0
        );
    }

    /// Regression for the smoke bug: when **every** commodity reports
    /// `R_k = 0` (buffers full) and `δ_k = 0` (no drain), λ has no
    /// non-zero coefficient anywhere in the LP, so `max λ` is
    /// mathematically unbounded. The naive solver returns
    /// `McmcfSolution::default()` via the `Unbounded` branch — which
    /// publishes an empty rate snapshot and breaks every DKMS that
    /// polls `/rate`. The solver must short-circuit and emit
    /// `λ = 0`, `r_k = δ_k = 0` for every commodity.
    #[test]
    fn all_buffers_full_no_drain_returns_zero_rate_not_unbounded() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        report(&reg, "dA", "dB", 4096.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let inputs = McmcfInputs::build(&t, &reg);
        let sol = McmcfSolver::new().solve(&inputs);
        // λ = 0 (nothing to fill). r_k = 0 for every commodity. The
        // rate map must include an entry per commodity (DKMSs polling
        // `/rate` rely on this) — empty `rates` would be a regression
        // back to the smoke bug.
        assert_eq!(sol.lambda, 0.0);
        assert_eq!(
            sol.rates.len(),
            inputs.commodities.len(),
            "rates map must hold one entry per commodity even when λ=0"
        );
        for (fid, r) in &sol.rates {
            assert_eq!(*r, 0.0, "{fid}: expected r=0, got {r}");
        }
    }

    /// Build a triangle topology used to exercise the lex
    /// refinement: 3 DKMSs at QKCs 1, 2, 3 with edge caps
    /// `(1,2) = (2,3) = 100` (the bottleneck pairs `(A,B)/(B,A)` and
    /// `(B,C)/(C,B)`) and `(1,3) = 1000` (a high-capacity edge that
    /// the LP only fills lightly at λ* — the residual is precisely
    /// what η_k consumes).
    fn triangle_with_slack() -> Topology {
        let mut t = Topology::default();
        for q in ["1", "2", "3"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        for nn in [("A", "1"), ("B", "2"), ("C", "3")] {
            let (name, q) = nn;
            t.upsert_orr(Orr {
                id: format!("o-{name}"),
                host: host(q.parse::<i64>().unwrap() + 10),
                qkc_id: q.into(),
            });
            t.upsert_dkms(Dkms {
                id: format!("dkms-{name}"),
                host: host(q.parse::<i64>().unwrap() + 20),
                tls_id: None,
                orr_id: format!("o-{name}"),
            });
        }
        link(&mut t, "1", "2", 100.0);
        link(&mut t, "2", "3", 100.0);
        link(&mut t, "1", "3", 1000.0);
        t
    }

    /// On a topology where some commodity's path has slack at λ*,
    /// the lex refinement picks up the residual: that commodity's
    /// `r_k > δ_k + λ*·R_k`. Verifies the slack-aware claim of
    /// paper §4.
    #[test]
    fn lex_refinement_uses_residual_capacity_on_free_path() {
        let t = triangle_with_slack();
        let reg = DemandRegistry::new();
        // All 6 ordered pairs empty + no drain → R_k = 4096 each.
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        assert!(sol.lambda > 0.0);

        // Bottleneck rates (commodities through cap=100 edges):
        // r ≈ λ·R_k stays near 50 (== 100 / (2 commodities sharing)).
        let r_ab = sol.rates[&flow_id("dkms-A", "dkms-B")];
        let r_ba = sol.rates[&flow_id("dkms-B", "dkms-A")];
        assert!(
            (r_ab - 50.0).abs() < 0.5 && (r_ba - 50.0).abs() < 0.5,
            "bottleneck rates r_AB={r_ab}, r_BA={r_ba} (expected ≈50 each)"
        );

        // Free-path rates (commodities through cap=1000 edge): no
        // longer 50 + 50 = 100 (which would leave 900 slack on
        // (1,3)). With lex, they consume the full edge. Each
        // commodity is also capped at η ≤ R/T_REPLAN = 819.2, so
        // the asymmetric distribution `[819.2, 80.8]` is possible
        // — we just assert the sum hits the edge cap.
        let r_ac = sol.rates[&flow_id("dkms-A", "dkms-C")];
        let r_ca = sol.rates[&flow_id("dkms-C", "dkms-A")];
        assert!(
            r_ac + r_ca > 900.0,
            "free-path total r_AC+r_CA = {} should approach edge cap 1000 thanks to η_k",
            r_ac + r_ca
        );
        assert!(
            r_ac + r_ca <= 1000.0 + 1.0,
            "free-path total must not exceed edge cap"
        );
    }

    /// Phase-1 guarantee: the lex refinement never decreases λ
    /// below the no-η optimum. Same topology as above; we'd be
    /// catastrophically broken if a future change traded λ for η.
    #[test]
    fn lex_refinement_preserves_lambda_optimum() {
        let t = triangle_with_slack();
        let reg = DemandRegistry::new();
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        // Bottleneck cuts (the cap=100 edges) carry 2 commodities
        // × R_k = 4096 each. Phase-1 λ = 100 / (2·4096) ≈ 0.01221.
        let expected = 100.0 / (2.0 * 4096.0);
        assert!(
            (sol.lambda - expected).abs() < 1e-6,
            "λ must stay at phase-1 optimum ≈ {expected}, got {}",
            sol.lambda
        );
    }

    /// On a topology with no slack — every edge saturated at λ* —
    /// every η_k stays at 0 (nothing to pick up). The augmented
    /// rate `r_k = δ_k + λ·R_k + η_k` reduces to the phase-1 rate
    /// and Proposition 4.1's exact synchronisation property holds.
    #[test]
    fn lex_refinement_is_no_op_when_no_slack() {
        // 2-node topology, 2 commodities, single shared edge.
        // Saturates at λ*; no slack anywhere.
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        // Same r_k as the no-η baseline: 50 each.
        let r_ab = sol.rates[&flow_id("dA", "dB")];
        let r_ba = sol.rates[&flow_id("dB", "dA")];
        assert!(
            (r_ab - 50.0).abs() < 1e-3 && (r_ba - 50.0).abs() < 1e-3,
            "no-slack topology must give 50/50 (no η contribution), got r_AB={r_ab}, r_BA={r_ba}"
        );
    }

    /// `R_k = 0` commodities have their η_k upper-bound clamped to
    /// zero in [`McmcfSolver::solve_eta`]. Regression for the
    /// implicit constraint: a full buffer must never receive
    /// residual fill, even when there's slack capacity it could
    /// theoretically claim. Combine with a free-path neighbour to
    /// make sure slack is available; the test then checks the full
    /// buffer's rate equals δ exactly.
    #[test]
    fn lex_refinement_respects_full_buffer_cap() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        // dA→dB: full + drain 20.  dB→dA: full + drain 0.
        report(&reg, "dA", "dB", 4096.0, 4096.0, 20.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        // All R_k = 0 triggers the short-circuit path: r_k = δ_k
        // exactly, no η.
        assert_eq!(sol.lambda, 0.0);
        assert!((sol.rates[&flow_id("dA", "dB")] - 20.0).abs() < 1e-6);
        assert!(sol.rates[&flow_id("dB", "dA")].abs() < 1e-6);
    }

    /// Build a diamond topology with two QKC-disjoint paths between
    /// `dA` and `dB`:
    ///
    /// ```text
    ///         m1
    ///        /  \
    ///       a    b   (qkc anchors for dA / dB)
    ///        \  /
    ///         m2
    /// ```
    ///
    /// Both `m1` and `m2` are pure-transit QKCs (no DKMS attached);
    /// the two parallel paths are `a→m1→b` and `a→m2→b`. Used by the
    /// multipath WCMP tests below.
    fn diamond_topology() -> Topology {
        let mut t = Topology::default();
        // Four QKCs.
        for q in ["1", "2", "11", "22"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        // Anchors at qkc 1 and qkc 2; m1=11, m2=22.
        t.upsert_orr(Orr {
            id: "o-1".into(),
            host: host(101),
            qkc_id: "1".into(),
        });
        t.upsert_orr(Orr {
            id: "o-2".into(),
            host: host(102),
            qkc_id: "2".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dA".into(),
            host: host(201),
            tls_id: None,
            orr_id: "o-1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(202),
            tls_id: None,
            orr_id: "o-2".into(),
        });
        // Diamond edges.
        link(&mut t, "1", "11", 100.0);
        link(&mut t, "11", "2", 100.0);
        link(&mut t, "1", "22", 100.0);
        link(&mut t, "22", "2", 100.0);
        t
    }

    /// Two parallel paths with equal capacity, single asymmetric
    /// commodity (dA→dB strongly demanded, dB→dA full): the LP must
    /// split the dA→dB flow across both paths to maximise λ — any
    /// single path would bottleneck at half the capacity. Verifies
    /// that `wcmp_from_edge_flows` correctly aggregates the split
    /// into a WCMP entry with two equally-weighted next-hops.
    #[test]
    fn diamond_topology_produces_multipath_wcmp_for_asymmetric_demand() {
        let t = diamond_topology();
        let reg = DemandRegistry::new();
        // dA→dB: empty buffer, high demand. dB→dA: full, no fill needed.
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let inputs = McmcfInputs::build(&t, &reg);
        let sol = McmcfSolver::new().solve(&inputs);
        assert!(sol.lambda > 0.0);

        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t);
        let hops = wcmp
            .get("1")
            .expect("qkc 1 has a WCMP table")
            .get("2")
            .expect("entry for dst=2");
        let mut hop_ids: Vec<u32> = hops.iter().map(|h| h.qkc_id).collect();
        hop_ids.sort();
        assert_eq!(hop_ids, vec![11, 22], "expected fan-out via m1 and m2");

        // Weights roughly equal (within 10 % of each other).
        let w1 = hops[0].weight as f64;
        let w2 = hops[1].weight as f64;
        let ratio = w1.max(w2) / w1.min(w2);
        assert!(
            ratio < 1.1,
            "expected ~equal weights (ratio≈1), got {ratio}"
        );
    }

    /// At a transit QKC, the WCMP entry for a destination has a
    /// single next-hop (the other end of the only outgoing edge
    /// toward that destination). Built on the same asymmetric
    /// scenario so qkc 11 actually carries flow.
    #[test]
    fn transit_node_has_single_next_hop_per_destination() {
        let t = diamond_topology();
        let reg = DemandRegistry::new();
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t);
        let hops = wcmp
            .get("11")
            .and_then(|table| table.get("2"))
            .expect("transit qkc 11 must have entry for dst=2");
        assert_eq!(hops.len(), 1, "transit node has one outgoing arc to dst");
        assert_eq!(hops[0].qkc_id, 2);
    }

    /// Quantisation guard: a tiny LP flow (e.g. numerical noise just
    /// above the FLOW_EPSILON cutoff) must still emit a usable
    /// weight of at least `WCMP_MIN_WEIGHT = 1`, never 0 (which the
    /// QKC sanitiser would strip).
    #[test]
    fn tiny_flow_quantises_to_at_least_weight_one() {
        let t = diamond_topology();
        // Single edge flow of 0.005 keys/s (above FLOW_EPSILON,
        // below 1/WCMP_WEIGHT_SCALE = 0.01).
        let flows = vec![EdgeFlow {
            flow_id: flow_id("dA", "dB"),
            src_qkc: "1".into(),
            dst_qkc: "11".into(),
            flow: 0.005,
        }];
        let wcmp = wcmp_from_edge_flows(&flows, &t);
        let hops = &wcmp["1"]["2"];
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].qkc_id, 11);
        assert!(
            hops[0].weight >= 1,
            "weight must not be 0, got {}",
            hops[0].weight
        );
    }

    /// `into_mcf_snapshot` exposes the WCMP table on the published
    /// snapshot so the forwarding push loop can read it.
    #[test]
    fn into_mcf_snapshot_includes_wcmp_table() {
        let t = diamond_topology();
        let reg = DemandRegistry::new();
        // Same asymmetric setup as the multipath WCMP test above so
        // the LP is forced to split.
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        let snap = sol.into_mcf_snapshot(&t);
        assert!(!snap.wcmp.is_empty(), "wcmp must be populated");
        let hops = &snap.wcmp["1"]["2"];
        assert_eq!(hops.len(), 2);
    }

    /// Reproducer of the smoke-test failure: 4 DKMSs anchored on
    /// leaves of a star topology (1 hub + 4 intermediates + 4
    /// leaves), with realistic buffer levels around 70 % full and
    /// zero drain. The smoke saw `LP infeasible — Unbounded` for
    /// this exact shape; this test pins the bug down to a single
    /// `#[test]` so we can iterate quickly.
    #[test]
    fn star_topology_with_realistic_demand_stays_bounded() {
        let mut t = Topology::default();
        // 9 QKCs: hub 0, intermediates 1-4, leaves 11/22/33/44.
        for q in ["0", "1", "2", "3", "4", "11", "22", "33", "44"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        // 8 edges (hub↔intermediates, intermediates↔leaves).
        for (a, b) in [
            ("0", "1"),
            ("0", "2"),
            ("0", "3"),
            ("0", "4"),
            ("1", "11"),
            ("2", "22"),
            ("3", "33"),
            ("4", "44"),
        ] {
            link(&mut t, a, b, 10_000.0);
        }
        // 4 ORRs + 4 DKMSs on the leaves.
        for nn in ["11", "22", "33", "44"] {
            t.upsert_orr(Orr {
                id: format!("orr-{nn}"),
                host: host(nn.parse().unwrap()),
                qkc_id: nn.into(),
            });
            t.upsert_dkms(Dkms {
                id: format!("dkms-{nn}"),
                host: host(nn.parse().unwrap()),
                tls_id: None,
                orr_id: format!("orr-{nn}"),
            });
        }
        // Demand reports for every ordered pair, 70 % full.
        let reg = DemandRegistry::new();
        for src in ["dkms-11", "dkms-22", "dkms-33", "dkms-44"] {
            for dst in ["dkms-11", "dkms-22", "dkms-33", "dkms-44"] {
                if src == dst {
                    continue;
                }
                report(&reg, src, dst, 7_232.0, 10_000.0, 0.0);
            }
        }
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        // Bottleneck = leaf↔intermediate edge, carries 6 commodities
        // (3 outgoing + 3 incoming). Edge cap = 10 000, R_k = 2 768
        // for every flow. So λ* = 10_000 / (6 · 2_768) ≈ 0.602 and
        // r_k = λ · R_k ≈ 1 666 keys/s per direction.
        assert!(
            sol.lambda > 0.0,
            "LP must produce λ > 0, got {}",
            sol.lambda
        );
        assert!(
            (sol.lambda - 10_000.0 / (6.0 * 2_768.0)).abs() < 1e-3,
            "expected λ ≈ 0.602, got {}",
            sol.lambda
        );
        // Every flow sees the same r_k (Prop. 4.1).
        for r in sol.rates.values() {
            assert!(
                (r - 10_000.0 / 6.0).abs() < 0.5,
                "expected r ≈ 1666.67, got {r}"
            );
        }
    }

    /// Build a 3×4 mesh of QKCs (12 nodes, 17 undirected edges) with
    /// DKMSs anchored at the four corners. The corner-to-corner
    /// commodities have multiple shortest paths through the interior
    /// of the mesh — the LP is forced to spread flow across them to
    /// maximise λ, which is exactly the scenario where WCMP
    /// fan-out earns its keep.
    ///
    /// QKC id naming: `(row * 10) + col`, so corners are `00`, `03`,
    /// `20`, `23`.
    fn mesh_3x4_topology() -> Topology {
        let mut t = Topology::default();
        for row in 0..3 {
            for col in 0..4 {
                let id = format!("{}{}", row, col);
                let host_id = (row * 4 + col + 1) as i64;
                t.upsert_qkc(Qkc {
                    id: id.clone(),
                    host: host(host_id),
                    kme_host: None,
                });
            }
        }
        // Horizontal edges (within each row).
        for row in 0..3 {
            for col in 0..3 {
                let a = format!("{}{}", row, col);
                let b = format!("{}{}", row, col + 1);
                link(&mut t, &a, &b, 1_000.0);
            }
        }
        // Vertical edges (between adjacent rows).
        for row in 0..2 {
            for col in 0..4 {
                let a = format!("{}{}", row, col);
                let b = format!("{}{}", row + 1, col);
                link(&mut t, &a, &b, 1_000.0);
            }
        }
        // DKMSs + ORRs at the 4 corners.
        for (name, qkc) in [("A", "00"), ("B", "03"), ("C", "20"), ("D", "23")] {
            t.upsert_orr(Orr {
                id: format!("o-{name}"),
                host: host(name.bytes().next().unwrap() as i64),
                qkc_id: qkc.into(),
            });
            t.upsert_dkms(Dkms {
                id: format!("dkms-{name}"),
                host: host(name.bytes().next().unwrap() as i64 + 100),
                tls_id: None,
                orr_id: format!("o-{name}"),
            });
        }
        t
    }

    /// On a 3×4 mesh with corner DKMSs, the diagonal commodity
    /// `(A, D)` between `(0,0)` and `(2,3)` has *no* single shortest
    /// path that can carry the LP's λ-optimal flow alone — the LP
    /// must spread across multiple paths, which surfaces as a WCMP
    /// entry with ≥2 next-hops at the source corner.
    #[test]
    fn mesh_3x4_diagonal_corner_pair_produces_multipath_wcmp() {
        let t = mesh_3x4_topology();
        // Asymmetric demand: A→D drives a hard demand (empty buffer),
        // the other commodities involving A and D are reported full
        // so they don't compete for capacity. The LP then has to
        // squeeze the single A→D commodity through every parallel
        // path at corner 00 to maximise λ.
        let reg = DemandRegistry::new();
        report(&reg, "dkms-A", "dkms-D", 0.0, 4096.0, 0.0);
        report(&reg, "dkms-D", "dkms-A", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-A", "dkms-B", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-B", "dkms-A", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-A", "dkms-C", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-C", "dkms-A", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-B", "dkms-C", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-C", "dkms-B", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-B", "dkms-D", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-D", "dkms-B", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-C", "dkms-D", 4096.0, 4096.0, 0.0);
        report(&reg, "dkms-D", "dkms-C", 4096.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        assert!(sol.lambda > 0.0, "LP must produce λ > 0");

        // WCMP at the source corner QKC `00`, for destination `23`:
        // both adjacent neighbours (`01` east and `10` south) carry
        // non-zero flow toward D.
        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t);
        let hops = wcmp
            .get("00")
            .expect("qkc 00 has wcmp")
            .get("23")
            .expect("entry for dst=23 from qkc 00");
        let hop_ids: Vec<u32> = hops.iter().map(|h| h.qkc_id).collect();
        // Adjacent neighbours of 00 are 01 (east) and 10 (south).
        assert!(
            hops.len() >= 2,
            "expected multipath fan-out, got {} next-hops: {:?}",
            hops.len(),
            hop_ids
        );
        assert!(
            hop_ids.contains(&1) && hop_ids.contains(&10),
            "expected fan-out via both 01 and 10, got {:?}",
            hop_ids
        );
    }

    /// Same mesh, full commodity grid (4 DKMSs × 3 peers = 12
    /// commodities, all with empty buffers). Verifies the LP scales
    /// to a non-trivial input set and produces consistent per-flow
    /// rates within Prop. 4.1's synchronisation window — at least
    /// when there is no slack capacity (any extra is absorbed by
    /// the lex refinement).
    #[test]
    fn mesh_3x4_full_grid_yields_consistent_rates() {
        let t = mesh_3x4_topology();
        let reg = DemandRegistry::new();
        let inputs = McmcfInputs::build(&t, &reg);
        assert_eq!(inputs.commodities.len(), 12, "4 DKMSs → 12 ordered pairs");
        let sol = McmcfSolver::new().solve(&inputs);
        assert!(sol.lambda > 0.0);
        // Every commodity gets a non-zero rate (no flow is squeezed
        // to zero in this symmetric setup).
        for c in &inputs.commodities {
            let r = sol.rates[&flow_id(&c.src_dkms, &c.dst_dkms)];
            assert!(
                r > 0.0,
                "commodity {}→{} should have positive rate, got {r}",
                c.src_dkms,
                c.dst_dkms
            );
        }
    }

    // --- smoke tests for the LP library (kept from the phase 1 stub) ----

    /// Smoke test: build the trivial LP `max x s.t. x ≤ 1, x ≥ 0` and
    /// solve it. Confirms `good_lp` + the configured backend are
    /// wired correctly and the workspace compiles against them.
    #[test]
    fn good_lp_smoke_solves_trivial_problem() {
        use good_lp::constraint;
        let mut vars = good_lp::ProblemVariables::new();
        let x = vars.add(variable().min(0.0).max(1.0));
        let solution = vars
            .maximise(x)
            .using(good_lp::default_solver)
            .with(constraint!(x <= 1.0))
            .solve()
            .expect("trivial LP must solve");
        let xv = solution.value(x);
        assert!((xv - 1.0).abs() < 1e-6, "expected x ≈ 1.0, got {xv}");
    }
}
