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
use std::sync::atomic::{AtomicU64, Ordering};

use good_lp::{
    variable, Constraint, Expression, ProblemVariables, Solution, Solver, SolverModel, Variable,
};
use tracing::{info, warn};

use common::security::KeyGrade;

use crate::{
    demand::{CommodityDemand, DemandRegistry},
    mcf::{flow_id, BufferRole, McfSnapshot, WcmpNextHop},
    topology::{edge_key, Topology},
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

    /// `(qkc_a, qkc_b) -> u_ij`, lexicographically ordered (a < b) so
    /// undirected edges aren't double-counted. QKD edges carry their
    /// distance-attenuated rate; PQC edges the capacity declared on the link
    /// (`EdgeMeta::pqc_capacity_keys_per_s`, default 10 000).
    pub edge_capacity: HashMap<(String, String), f64>,

    /// `dkms_id -> qkc_id` for every DKMS in the topology that has
    /// a resolvable QKC anchor.
    pub dkms_to_qkc: HashMap<String, String>,

    /// Canonical `(qkc_a, qkc_b)` (a < b) of every **PQC** edge. The LP
    /// forbids QKD-grade commodities from using these arcs (their flow
    /// variables are pinned to 0), so a QKD-grade key never traverses a
    /// PQC link. QKD edges are absent from this set.
    pub pqc_edges: HashSet<(String, String)>,
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

        // Edge capacity table (canonical key form a < b). QKD edges carry
        // their distance-attenuated rate; PQC edges get an effectively
        // unbounded sentinel so they never bound λ (a true ∞ would make the
        // max-λ LP unbounded → solver returns Unbounded → zero fallback,
        // the opposite of intended).
        let mut edge_capacity: HashMap<(String, String), f64> = HashMap::new();
        let mut pqc_edges: HashSet<(String, String)> = HashSet::new();
        for ((a, b), meta) in &topology.edges {
            let cap = meta.capacity_keys_per_second();
            if cap > 0.0 {
                edge_capacity.insert((a.clone(), b.clone()), cap);
                if meta.is_pqc() {
                    pqc_edges.insert((a.clone(), b.clone()));
                }
            }
        }

        // QKD-subgraph components: a pair routes QKD-grade iff its two QKCs
        // share a component (a strictly-QKD path connects them). Pairs that
        // are only PQC-connected get grade PQC so the LP may use PQC arcs —
        // exactly the `qkd_prefer` default (QKD where reachable, else PQC).
        let qkd_comp = topology.qkd_components();

        // Commodity set: for every ordered pair (src, dst) in distinct QKCs we
        // emit up to TWO commodities, one per key grade:
        //   * QKD-grade — only when a QKD path connects the pair (else it could
        //     never route and would force the shared λ to 0). Bootstrapped with
        //     zero demand on every QKD-connected pair.
        //   * PQC-grade — emitted on a QKD-disconnected pair (its natural,
        //     bootstrapped grade) OR on a QKD-connected pair only when actual
        //     PQC-grade demand was reported (`no_worry` traffic). Routes over the
        //     full graph.
        // So a fully-QKD-connected topology with no `no_worry` demand keeps one
        // commodity per pair (the LP stays the size verified earlier); PQC
        // commodities appear only where genuinely needed.
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
                    // Same QKC anchor — no network flow needed between them.
                    continue;
                }
                let qkd_connected = matches!(
                    (qkd_comp.get(src_qkc), qkd_comp.get(dst_qkc)),
                    (Some(x), Some(y)) if x == y
                );
                let natural = if qkd_connected {
                    KeyGrade::Qkd
                } else {
                    KeyGrade::Pqc
                };
                for grade in [KeyGrade::Qkd, KeyGrade::Pqc] {
                    // A QKD-grade commodity is impossible without a QKD path.
                    if grade == KeyGrade::Qkd && !qkd_connected {
                        continue;
                    }
                    match registry.get(src.as_str(), dst.as_str(), grade) {
                        Some(mut d) => {
                            d.grade = grade; // authoritative
                            commodities.push(d);
                        }
                        None if grade == natural => {
                            // Bootstrap the natural grade with zero demand.
                            commodities.push(CommodityDemand {
                                src_dkms: (*src).clone(),
                                dst_dkms: (*dst).clone(),
                                level: 0.0,
                                capacity: DEFAULT_BUFFER_CAPACITY,
                                drain_rate: 0.0,
                                timestamp_ms: 0,
                                grade,
                            });
                        }
                        // Non-natural grade with no reported demand → no commodity.
                        None => {}
                    }
                }
            }
        }
        commodities.sort_by(|a, b| {
            (a.src_dkms.as_str(), a.dst_dkms.as_str(), a.grade.as_str()).cmp(&(
                b.src_dkms.as_str(),
                b.dst_dkms.as_str(),
                b.grade.as_str(),
            ))
        });

        Self {
            commodities,
            edge_capacity,
            dkms_to_qkc,
            pqc_edges,
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
    /// Grade of the commodity this flow belongs to. Lets the snapshot build a
    /// QKD-only WCMP table (grade == Qkd flows) distinct from the full table.
    pub grade: KeyGrade,
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

    /// `flow_id → r_k`, **aggregated over grades** (a pair carrying both a
    /// QKD-grade and PQC-grade commodity sums them). Includes flows with
    /// `r_k = 0` so callers can distinguish "known commodity, no rate" from
    /// "unknown".
    pub rates: HashMap<String, f64>,

    /// `(flow_id, grade) → r_k` — the per-grade breakdown of [`Self::rates`].
    pub rates_grade: HashMap<(String, KeyGrade), f64>,
}

/// Flows smaller than this in absolute value get truncated to 0.
/// Both for the edge-flow support and for the rate view. Picks up
/// solver numerical noise without losing real allocations — the
/// smallest meaningful rate in practice is `δ_k > 0`, which is at
/// least 1 key/s for any active SAE.
pub(crate) const FLOW_EPSILON: f64 = 1e-6;

/// Horizon (seconds) over which a single LP solve is considered
/// authoritative. The LP delivers `r_k = δ_k + λ·R_k + η_k` for the
/// commodity; capping `η_k ≤ R_k / T_REPLAN` ensures the buffer
/// can't overflow before the next solve replaces these rates with
/// fresh ones. Matches the SDN's default `mcf_period_ms = 5000`.
pub(crate) const T_REPLAN_SECONDS: f64 = 5.0;

/// Penalty multiplier on `Σ σ_k` in phase-1 LP. With `λ` in the
/// range `[0, 10]` and `σ_k` in the range `[0, max_drain]` (≈ 1e3),
/// a coefficient of 1e3 makes the LP minimise σ absolutely before
/// touching λ — the strict lex order between "deliver δ" and
/// "fill at λ" required by paper §6's relaxation.
const LAMBDA_SLACK_PENALTY: f64 = 1e3;

/// Carga de una arista tras la fase 1: `((qkc_a, qkc_b), flujo, capacidad)`.
/// Vacío salvo con [`lp_diag_enabled`].
type EdgeLoads = Vec<((String, String), f64, f64)>;

/// Diagnóstico del LP: `SDN_LOG_LP_DIAG=1`.
///
/// Saca por recompute la utilización de cada arista, que es lo único que
/// distingue "no hay capacidad" de "la hay pero concentrada en unas pocas
/// aristas". Cuesta recuperar 2·|K| variables por arista del solver, así que
/// va bajo interruptor: en marcha normal basta con `slack_*`, que es gratis.
fn lp_diag_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("SDN_LOG_LP_DIAG")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false)
    })
}

/// LP backend, selected once per process via the `SDN_SOLVER` env var:
/// `microlp` (default — the historical pure-Rust simplex, exact basic
/// solutions but dense and single-core: stalls at N≥40 / ≥1560
/// commodities) or `clarabel` (pure-Rust interior-point: approximate
/// vertex values but scales past the microlp wall).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LpBackend {
    Microlp,
    Clarabel,
    Highs,
}

fn lp_backend() -> LpBackend {
    static BACKEND: std::sync::OnceLock<LpBackend> = std::sync::OnceLock::new();
    *BACKEND.get_or_init(|| {
        // Default `highs` desde 2026-08-25: microlp falseaba la fase 2 en
        // el 97 % de los solves ya a N=10/90 commodities bajo carga (CESGA
        // job 9266817) y a N=20 era ~total. microlp y clarabel siguen
        // seleccionables para comparar y como salida de emergencia si un
        // entorno no puede compilar HiGHS (C++, necesita cmake).
        let backend = match std::env::var("SDN_SOLVER") {
            Ok(v) if v.eq_ignore_ascii_case("clarabel") => LpBackend::Clarabel,
            Ok(v) if v.eq_ignore_ascii_case("microlp") => LpBackend::Microlp,
            Ok(v) if v.is_empty() || v.eq_ignore_ascii_case("highs") => LpBackend::Highs,
            Ok(v) => {
                warn!(
                    value = %v,
                    "unknown SDN_SOLVER (expected microlp|clarabel|highs); using highs"
                );
                LpBackend::Highs
            }
            Err(_) => LpBackend::Highs,
        };
        // info-level so deployed logs (RUST_LOG=info) positively confirm
        // which backend is engaged — a lost env var silently falling back
        // to microlp is otherwise indistinguishable from clarabel running.
        info!(backend = ?backend, "SDN LP backend selected");
        backend
    })
}

/// Build the model on `solver`, add every constraint, solve, and
/// return the values of `wanted` in order. Each backend has its own
/// concrete model/solution types, so this is the monomorphisation
/// point — callers stay backend-agnostic by consuming plain `f64`s.
fn solve_lp<S: Solver>(
    solver: S,
    vars: ProblemVariables,
    objective: Expression,
    constraints: Vec<Constraint>,
    wanted: &[Variable],
) -> Result<Vec<f64>, String> {
    let mut model = vars.maximise(objective).using(solver);
    for c in constraints {
        model = model.with(c);
    }
    match model.solve() {
        Ok(sol) => Ok(wanted.iter().map(|v| sol.value(*v)).collect()),
        Err(e) => Err(e.to_string()),
    }
}

/// Clarabel-specific solve path. The generic [`solve_lp`] can't be
/// used here because good_lp's clarabel backend maps the
/// `DualInfeasible`/`AlmostDualInfeasible` exits (primal unbounded)
/// to `Ok` — for those, `solution.x` is an unbounded-direction
/// CERTIFICATE, not a feasible point, and publishing it as rates
/// would push garbage to every DKMS (microlp returns `Err` for the
/// same condition and falls back to zero rates). We re-check the raw
/// Clarabel status after the solve and accept only genuine optima.
fn solve_lp_clarabel(
    vars: ProblemVariables,
    objective: Expression,
    constraints: Vec<Constraint>,
    wanted: &[Variable],
) -> Result<Vec<f64>, String> {
    use clarabel::solver::SolverStatus;

    let mut model = vars.maximise(objective).using(good_lp::clarabel);
    // good_lp pins tol_feas to 1e-9 — tighter than Clarabel's own 1e-8
    // default and needlessly stall-prone on large degenerate MCF LPs
    // (rates are keys/s magnitudes, filtered at FLOW_EPSILON=1e-6).
    model.settings().tol_feas(1e-8);
    for c in constraints {
        model = model.with(c);
    }
    match model.solve() {
        Ok(sol) => {
            match sol.inner().status {
                SolverStatus::Solved => {}
                // Reduced-tolerance exit (1e-4 feas): still a feasible
                // point near the optimum — degraded rate accuracy beats
                // publishing zero rates for a whole replan period.
                SolverStatus::AlmostSolved => {
                    warn!("clarabel exited AlmostSolved; using reduced-accuracy solution");
                }
                other => {
                    return Err(format!(
                        "clarabel terminated with non-optimal status {other:?}"
                    ))
                }
            }
            Ok(wanted.iter().map(|v| sol.value(*v)).collect())
        }
        Err(e) => Err(e.to_string()),
    }
}

fn solve_lp_backend(
    vars: ProblemVariables,
    objective: Expression,
    constraints: Vec<Constraint>,
    wanted: &[Variable],
) -> Result<Vec<f64>, String> {
    match lp_backend() {
        LpBackend::Microlp => solve_lp(good_lp::microlp, vars, objective, constraints, wanted),
        LpBackend::Clarabel => solve_lp_clarabel(vars, objective, constraints, wanted),
        LpBackend::Highs => solve_lp(good_lp::highs, vars, objective, constraints, wanted),
    }
}

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

        // Aggregate rates (summed over grades): flat + ENC/DEC mirror.
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

        // Per-grade rates: the DKMS fills its (peer, grade) buffers from these.
        for ((flow_id_str, grade), rate) in self.rates_grade.iter() {
            let Some((src, dst)) = flow_id_str.split_once("->") else {
                continue;
            };
            snap.rates_by_dkms_grade
                .entry(src.to_string())
                .or_default()
                .insert((dst.to_string(), BufferRole::EncKeys, *grade), *rate);
            snap.rates_by_dkms_grade
                .entry(dst.to_string())
                .or_default()
                .insert((src.to_string(), BufferRole::DecKeys, *grade), *rate);
        }

        // WCMP: derivada de topología+capacidades, NUNCA de los edge_flows
        // del LP. La ruta debe existir siempre y cambiar al ritmo de la
        // topología; las degeneraciones del solver (cortocircuito de buffers
        // llenos, falsa infeasibilidad de microlp, clarabel sin fase 2)
        // quedan así confinadas a las tasas, donde son recuperables. La tabla
        // QKD-only usa el subgrafo QKD para que un frame de ese grado jamás
        // pise un enlace PQC.
        snap.wcmp = wcmp_from_topology(topology, None);
        snap.wcmp_qkd = wcmp_from_topology(topology, Some(KeyGrade::Qkd));
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
/// `grade_filter`: when `Some(g)`, only edge-flows of grade `g` contribute
/// (used to build the QKD-only table); `None` aggregates all grades.
/// keys/s → peso WCMP entero.
///
/// Saturar en vez de `as u32`, que sobre un valor mayor que 2³² **envuelve
/// módulo 2³²** en silencio y convierte un reparto en un resto de división.
/// No es hipotético: con las aristas PQC a capacidad centinela (1e9) los
/// valores escalados salen enormes — en el testbed se vieron pesos de
/// 2 755 359 744, que aún caben, pero por encima de ~42,9 M claves/s ya no.
/// Los pesos son proporciones relativas, así que topar en `u32::MAX` no
/// cambia el sentido. `clamp` ya lleva ±∞ a los extremos correctos; solo el
/// NaN necesita salida propia, porque `clamp` lo propaga y luego `as u32` lo
/// volvería un 0 que el saneador del QKC descarta.
fn quantise_weight(keys_per_second: f64) -> u32 {
    let scaled = (keys_per_second * WCMP_WEIGHT_SCALE).round();
    if scaled.is_nan() {
        WCMP_MIN_WEIGHT
    } else {
        scaled.clamp(f64::from(WCMP_MIN_WEIGHT), f64::from(u32::MAX)) as u32
    }
}

/// WCMP derivada de los flujos del LP — **no cableada al snapshot**.
///
/// Queda como gancho para el refinamiento por demanda: si algún día se quiere
/// sesgar la ruta con la solución del LP, debe aplicarse *encima* de
/// [`wcmp_from_topology`] y solo cuando el solver traiga una solución sana,
/// nunca como fuente única — encaminar con la salida del LP heredaba sus
/// casos degenerados y dejó el multipath inactivo en todo lo medido hasta
/// 2026-08-24.
pub fn wcmp_from_edge_flows(
    edge_flows: &[EdgeFlow],
    topology: &Topology,
    grade_filter: Option<KeyGrade>,
) -> HashMap<String, HashMap<String, Vec<WcmpNextHop>>> {
    // (transit_qkc, commodity_dst_qkc) → (neighbour → cumulative flow)
    let mut buckets: HashMap<(String, String), HashMap<String, f64>> = HashMap::new();
    for ef in edge_flows {
        if let Some(g) = grade_filter {
            if ef.grade != g {
                continue;
            }
        }
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
                Some(WcmpNextHop {
                    qkc_id,
                    weight: quantise_weight(flow),
                })
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

/// Tablas WCMP derivadas **solo de la topología y las capacidades** — la
/// fuente de encaminamiento del sistema.
///
/// Existe porque encaminar con la salida del LP acoplaba el *por dónde* al
/// *cuánto*, y el encaminamiento heredaba todos los casos degenerados del
/// reparto. Tres caminos independientes acababan en `edge_flows` vacío — el
/// cortocircuito de buffers llenos (que con el rebasamiento del generador era
/// el estado permanente), la falsa infeasibilidad de microlp en fase 2 a
/// N≥20, y el backend clarabel, que apaga la fase 2 por diseño — así que el
/// multipath del diseño no llegó a estar activo en ningún despliegue medido
/// (2026-08-24): el QKC caía a camino más corto, unas aristas se secaban y
/// otras quedaban ociosas. Y aunque el LP funcionara, recalcular la ruta cada
/// 5 s desde el nivel instantáneo de los buffers haría oscilar las tablas: una
/// ruta debe cambiar al ritmo de la topología, no al de los buffers.
///
/// Reparto: para cada destino se hace BFS por saltos y cada nodo reparte
/// entre sus vecinos **estrictamente más cercanos** al destino — la distancia
/// decrece en cada salto, así que no hay bucles por construcción, aunque cada
/// QKC decida por su cuenta. El peso de cada next-hop es el cuello de botella
/// (capacidad mínima) del mejor camino descendente que pasa por él, con el
/// modelo de siempre: `r0·10^(−α·d/10)` en QKD y el centinela en PQC — en una
/// malla mixta los enlaces PQC (sin coste en clave) absorben el tráfico, y en
/// una malla homogénea el reparto es proporcional a la fibra.
///
/// Determinista: mismo snapshot ⇒ misma tabla, byte a byte. El refinamiento
/// por demanda ([`wcmp_from_edge_flows`]) queda como sesgo futuro **encima**
/// de esta base, nunca como sustituto.
pub fn wcmp_from_topology(
    topology: &Topology,
    grade_filter: Option<KeyGrade>,
) -> HashMap<String, HashMap<String, Vec<WcmpNextHop>>> {
    // Adyacencia con capacidad, filtrada por grado. Se parte de `edges` y no
    // de `graph`: es donde está la metadata, y así el filtro QKD sale gratis.
    let mut adj: HashMap<&str, Vec<(&str, f64)>> = HashMap::new();
    for ((a, b), meta) in &topology.edges {
        if grade_filter == Some(KeyGrade::Qkd) && meta.is_pqc() {
            continue;
        }
        let cap = meta.capacity_keys_per_second();
        adj.entry(a.as_str()).or_default().push((b.as_str(), cap));
        adj.entry(b.as_str()).or_default().push((a.as_str(), cap));
    }
    // Orden estable en la adyacencia para que el resultado sea determinista.
    for nbrs in adj.values_mut() {
        nbrs.sort_by(|x, y| x.0.cmp(y.0));
    }

    let mut dests: Vec<&str> = topology.qkcs.keys().map(String::as_str).collect();
    dests.sort_unstable();

    let mut out: HashMap<String, HashMap<String, Vec<WcmpNextHop>>> = HashMap::new();
    for dst in dests {
        // BFS de saltos desde el destino.
        let mut dist: HashMap<&str, u32> = HashMap::new();
        dist.insert(dst, 0);
        let mut queue = std::collections::VecDeque::from([dst]);
        let mut order: Vec<&str> = vec![dst]; // por distancia creciente
        while let Some(v) = queue.pop_front() {
            let dv = dist[v];
            for (n, _) in adj.get(v).map(Vec::as_slice).unwrap_or(&[]) {
                if !dist.contains_key(n) {
                    dist.insert(n, dv + 1);
                    order.push(n);
                    queue.push_back(n);
                }
            }
        }
        // Camino más ancho descendente: `width(v)` = mejor cuello de botella
        // hasta `dst` usando solo vecinos a distancia dv−1. `order` va por
        // distancia creciente, así que los width de dv−1 ya están.
        let mut width: HashMap<&str, f64> = HashMap::new();
        width.insert(dst, f64::INFINITY);
        for v in order.iter().skip(1) {
            let dv = dist[v];
            let mut best = 0.0f64;
            let mut hops: Vec<WcmpNextHop> = Vec::new();
            for (n, cap) in adj.get(v).map(Vec::as_slice).unwrap_or(&[]) {
                if dist.get(n) != Some(&(dv - 1)) {
                    continue; // solo estrictamente más cerca: sin bucles
                }
                let through = cap.min(width[n]);
                best = best.max(through);
                if let Ok(qkc_id) = n.parse::<u32>() {
                    hops.push(WcmpNextHop {
                        qkc_id,
                        weight: quantise_weight(through),
                    });
                }
            }
            width.insert(v, best);
            if !hops.is_empty() {
                hops.sort_by_key(|h| h.qkc_id);
                out.entry((*v).to_string())
                    .or_default()
                    .insert(dst.to_string(), hops);
            }
        }
    }
    out
}

/// Build the per-commodity flow-variable matrix `x[k][arc]` shared by both LP
/// phases, **pinning to 0** the cells `(k, arc)` where commodity `k` is
/// QKD-grade and `arc` is a PQC arc. A fixed `[0, 0]` bound (rather than
/// dropping the variable) keeps the dense `x[k][arc]` indexing — and thus the
/// conservation/capacity constraint builders — untouched; the LP simply never
/// routes QKD-grade flow over a PQC link.
///
/// Invariant relied upon elsewhere: a QKD-grade commodity is only ever created
/// for a QKD-connected pair (see `McmcfInputs::build` + the DKMS never reports
/// `strict_qkd` demand for a QKD-disconnected pair), so pinning its PQC arcs to
/// 0 never makes its conservation infeasible — there is always a QKD path — and
/// the shared global `λ` cannot collapse.
fn build_flow_vars(
    vars: &mut ProblemVariables,
    active: &[&CommodityDemand],
    arcs: &[(String, String)],
    pqc_edges: &HashSet<(String, String)>,
) -> Vec<Vec<Variable>> {
    let arc_is_pqc: Vec<bool> = arcs
        .iter()
        .map(|(a, b)| pqc_edges.contains(&edge_key(a, b)))
        .collect();
    active
        .iter()
        .map(|c| {
            (0..arcs.len())
                .map(|a_idx| {
                    let def = variable().min(0.0);
                    let def = if c.grade == KeyGrade::Qkd && arc_is_pqc[a_idx] {
                        def.max(0.0) // pin to 0: QKD-grade never uses a PQC arc
                    } else {
                        def
                    };
                    vars.add(def)
                })
                .collect()
        })
        .collect()
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
            let mut rates_grade: HashMap<(String, KeyGrade), f64> =
                HashMap::with_capacity(active.len());
            for c in &active {
                let fid = flow_id(&c.src_dkms, &c.dst_dkms);
                *rates.entry(fid.clone()).or_insert(0.0) += c.drain_rate;
                rates_grade.insert((fid, c.grade), c.drain_rate);
            }
            // Esta rama sale ANTES de las dos fases del LP. Desde 2026-08-24
            // eso ya solo afecta a las TASAS (λ=0 ⇒ compensar drenaje y nada
            // más, que con los buffers llenos es la respuesta correcta): la
            // ruta WCMP se deriva de la topología en `into_mcf_snapshot`, así
            // que esta rama ya no puede dejar a la red sin multipath, que es
            // lo que hacía — el generador rebasaba `capacity_per_peer`
            // (4096 → 4107), `remaining()` saturaba a 0 en las 90 commodities
            // y el forwarding caía a camino más corto permanentemente.
            // `overfull` debe ser 0 desde que el generador capa el lote al
            // hueco; si reaparece, algo vuelve a rebasar el buffer.
            let overfull = active.iter().filter(|c| c.level > c.capacity).count();
            let drain_positive = active.iter().filter(|c| c.drain_rate > 0.0).count();
            let drain_total: f64 = active.iter().map(|c| c.drain_rate).sum();
            // `drain_positive` cuenta `> 0.0` estricto, y la EWMA del DKMS
            // nunca vuelve a 0.0 exacto tras el primer request — por sí solo
            // dice "hubo tráfico SAE alguna vez", no "hay demanda ahora". La
            // que discrimina es `drain_total`: Σδ≈0 con buffers llenos es el
            // reposo sano de la malla y se imprime a info con throttle;
            // Σδ real (≥ 1 clave/s) o un overfull sí ameritan el warn. Antes
            // el estado "todo lleno y sin drain jamás registrado" no
            // imprimía NADA, y el `drain_positive=0` de CESGA hubo que
            // inferirlo del silencio del diag.
            static ALL_FULL_TICKS: AtomicU64 = AtomicU64::new(0);
            let ticks = ALL_FULL_TICKS.fetch_add(1, Ordering::Relaxed);
            if overfull > 0 || drain_total >= 1.0 {
                warn!(
                    n_commodities = active.len(),
                    overfull,
                    drain_positive,
                    drain_total = format!("{drain_total:.1}"),
                    "mcmcf.diag: todas las commodities sin hueco de buffer; λ=0 (solo se \
                     compensa el drenaje; la ruta WCMP no depende de esta rama)",
                );
            } else if ticks.is_multiple_of(12) {
                info!(
                    n_commodities = active.len(),
                    drain_positive,
                    drain_total = format!("{drain_total:.3}"),
                    "mcmcf.diag: buffers llenos y sin demanda SAE apreciable; λ=0 en reposo",
                );
            }
            return McmcfSolution {
                lambda: 0.0,
                edge_flows: Vec::new(),
                rates,
                rates_grade,
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

        // ----- Phase 0: max t — el suelo max-min de la escasez ----------
        //
        // `max λ − M·Σσ` minimiza la SUMA de drenaje desatendido, y a la
        // suma le da igual repartir que concentrar: el símplex devuelve
        // vértices que dejan commodities enteras con σ_k = δ_k (cero
        // servicio) mientras otras reciben el 100 % — medido en CESGA
        // 2026-08-25 (jobs 9265640/9266817): pares fijos al 100 % de ceros
        // durante los 300 s de carga. La pre-fase calcula la fracción común
        // t* servible a TODOS (max-min de primer nivel) y la fase 1 corre
        // después con σ_k acotado a (1−t*)·δ_k: ninguna commodity puede ya
        // quedar por debajo del suelo común. Con `SDN_DISABLE_SLACK_VARS=1`
        // no hay σ que acotar y la pre-fase se omite.
        let slack_disabled = std::env::var("SDN_DISABLE_SLACK_VARS")
            .map(|v| v != "0" && !v.is_empty())
            .unwrap_or(false);
        let t_floor = if slack_disabled {
            0.0
        } else {
            self.solve_common_fraction(&active, &arcs, &arc_idx, &node_set, &undirected, inputs)
                // Un pelo por debajo del óptimo para que el redondeo del
                // backend no convierta el suelo en infeasibilidad.
                .map(|t| (t - 1e-6).clamp(0.0, 1.0))
                .unwrap_or(0.0)
        };

        // ----- Phase 1: solve max λ − M·Σ σ_k ---------------------------
        let phase1 = self
            .solve_lambda(
                &active,
                &arcs,
                &arc_idx,
                &node_set,
                &undirected,
                inputs,
                t_floor,
            )
            .or_else(|| {
                if t_floor > 0.0 {
                    // El suelo viene de otro solve del mismo backend: si el
                    // ruido numérico lo dejó una pizca alto, mejor perder el
                    // suelo en este recompute que perder todas las rates.
                    warn!(
                        t_floor = format!("{t_floor:.6}"),
                        "fase 1 infeasible con el suelo max-min; reintento sin suelo",
                    );
                    self.solve_lambda(
                        &active,
                        &arcs,
                        &arc_idx,
                        &node_set,
                        &undirected,
                        inputs,
                        0.0,
                    )
                } else {
                    None
                }
            });
        let (lambda_star, sigmas, edge_loads) = match phase1 {
            Some(v) => v,
            None => return zero_rate_fallback(&active),
        };

        // Por qué λ sale lo que sale.
        //
        // `max λ − 1000·Σσ_k` minimiza la holgura ANTES de tocar λ, así que
        // una sola commodity que no se pueda servir aplasta λ a cero para
        // TODA la red — y con λ=0 no hay llenado proactivo en ningún par,
        // sólo se compensa el drenaje medido. Distinguir eso de "no hay
        // capacidad" exige ver las dos cosas juntas: cuánta holgura queda sin
        // servir y cómo de llenas están las aristas. Medido el 2026-08-24:
        // λ=0 sostenido con la demanda total (5 143 claves/s a 1,93 saltos)
        // cabiendo de sobra en la fibra (22 241), lo que apunta a saturación
        // LOCAL de unas pocas aristas y no a falta de capacidad global.
        let n_slack = sigmas.iter().filter(|s| **s > FLOW_EPSILON).count();
        let slack_total: f64 = sigmas.iter().sum();
        if n_slack > 0 || lambda_star <= FLOW_EPSILON {
            let mut top: Vec<String> = Vec::new();
            if !edge_loads.is_empty() {
                let mut by_util: Vec<(f64, &(String, String), f64)> = edge_loads
                    .iter()
                    .map(|(k, load, cap)| (load / cap.max(f64::EPSILON), k, *load))
                    .collect();
                by_util.sort_by(|a, b| b.0.total_cmp(&a.0));
                top = by_util
                    .iter()
                    .take(4)
                    .map(|(u, k, load)| format!("{}-{}:{:.0}%({:.0})", k.0, k.1, u * 100.0, load))
                    .collect();
            }
            warn!(
                lambda = lambda_star,
                n_commodities = active.len(),
                slack_commodities = n_slack,
                slack_total = format!("{slack_total:.1}"),
                edges_most_loaded = ?top,
                "mcmcf.diag: λ colapsado o con holgura sin servir \
                 (SDN_LOG_LP_DIAG=1 para la utilización por arista)",
            );
        }

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
        // Phase 2 is additionally FORCED OFF on the Clarabel backend,
        // regardless of the env var. Interior-point methods converge to
        // the analytic centre of the optimal face, and phase 2's
        // objective (max Σ η) leaves the routing degenerate: every
        // cycle with capacity slack carries real-magnitude circulating
        // flow (including the 2-cycle on each edge), far above
        // FLOW_EPSILON. Reading those x values back as edge_flows
        // builds WCMP tables with loop-forming next-hops. Pinning the
        // (approximate) λ* into phase-2 equalities also flaps to
        // false-infeasible whenever λ* comes back superoptimal
        // (AlmostSolved). Phase-1-only means empty edge_flows → the
        // forwarding push falls back to topology shortest-path, the
        // same behaviour production deploys already choose via
        // SDN_DISABLE_LEX_REFINEMENT=1.
        let lex_disabled = lp_backend() == LpBackend::Clarabel
            || std::env::var("SDN_DISABLE_LEX_REFINEMENT")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false);
        let phase2 = if lex_disabled {
            Phase2Output {
                eta_values: vec![0.0; active.len()],
                edge_flows: Vec::new(),
                fallback: false,
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
        let mut rates_grade: HashMap<(String, KeyGrade), f64> =
            HashMap::with_capacity(active.len());
        let mut total_eta = 0.0;
        let mut total_sigma = 0.0;
        let mut drain_in = 0.0;
        let mut drain_delivered_total = 0.0;
        let mut fill_total = 0.0;
        let mut rates_zero = 0usize;
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
            drain_in += c.drain_rate;
            drain_delivered_total += delivered_drain;
            fill_total += lambda_star * c.remaining();
            let fid = flow_id(&c.src_dkms, &c.dst_dkms);
            let r_k_clean = if r_k.abs() < FLOW_EPSILON { 0.0 } else { r_k };
            if r_k_clean == 0.0 {
                rates_zero += 1;
            }
            *rates.entry(fid.clone()).or_insert(0.0) += r_k_clean;
            rates_grade.insert((fid, c.grade), r_k_clean);
        }

        // Un par (src,dst) repetido entre las commodities activas es una
        // entrada fantasma: si un DKMS cambia el grade con el que reporta a
        // un peer (flip de qkd_available), la del grade antiguo sigue hasta
        // que el sweeper la retire (`demand_ttl_secs`), y mientras tanto el
        // par cuenta doble. Sostenido en el tiempo, ya no es transitorio.
        let mut pair_counts: HashMap<(&str, &str), usize> = HashMap::new();
        for c in &active {
            *pair_counts
                .entry((c.src_dkms.as_str(), c.dst_dkms.as_str()))
                .or_insert(0) += 1;
        }
        let dup_pairs = pair_counts.values().filter(|&&n| n > 1).count();

        // La descomposición de r_k por solve, a info: es la única línea con
        // la que las muestras de `/rate` a cero se pueden atribuir a su
        // causa — reposo (drain_in≈0), σ comiéndose el drain (sigma alto con
        // λ=0), o la fase 2 caída (eta_fallback) — sin correlacionar a mano
        // tres logs. A la cadencia del recompute (5 s) cuesta lo mismo que
        // un `generator.state` y es lo que el muestreador de las campañas
        // recoge. El 68 % de muestras a cero de CESGA quedó sin atribuir
        // exactamente por no tener esto.
        info!(
            n_commodities = active.len(),
            n_arcs = arcs.len(),
            lambda = format!("{lambda_star:.6}"),
            // El suelo max-min de la fase 0: fracción del drenaje que TODA
            // commodity tiene garantizada. 0.0 = sin suelo (sin drenaje, o
            // fase 0 caída).
            t_floor = format!("{t_floor:.4}"),
            drain_in = format!("{drain_in:.1}"),
            drain_delivered = format!("{drain_delivered_total:.1}"),
            sigma_total = format!("{total_sigma:.1}"),
            fill_total = format!("{fill_total:.1}"),
            eta_total = format!("{total_eta:.1}"),
            rates_zero,
            eta_fallback = phase2.fallback,
            dup_pairs,
            n_edge_flows = phase2.edge_flows.len(),
            backend = ?lp_backend(),
            "mcmcf.solve: r_k = (δ−σ) + λ·R + η",
        );

        McmcfSolution {
            lambda: lambda_star,
            edge_flows: phase2.edge_flows,
            rates,
            rates_grade,
        }
    }

    /// Phase 0: `max t` — la fracción común del drenaje servible a TODAS las
    /// commodities a la vez (max-min de primer nivel). Solo participan las
    /// commodities con δ_k > 0: cada una debe encaminar exactamente `t·δ_k`
    /// bajo las mismas restricciones de capacidad compartida que el resto de
    /// fases. `None` si no hay drenaje que repartir o si el backend falla
    /// (el caller trata ambos como "sin suelo", nunca como error fatal).
    fn solve_common_fraction(
        &self,
        active: &[&CommodityDemand],
        arcs: &[(String, String)],
        arc_idx: &HashMap<(String, String), usize>,
        node_set: &HashSet<String>,
        undirected: &[((String, String), f64)],
        inputs: &McmcfInputs,
    ) -> Option<f64> {
        let drainers: Vec<&CommodityDemand> = active
            .iter()
            .filter(|c| c.drain_rate > FLOW_EPSILON)
            .copied()
            .collect();
        if drainers.is_empty() {
            return None;
        }
        let mut vars = ProblemVariables::new();
        let t = vars.add(variable().min(0.0).max(1.0));
        let x: Vec<Vec<Variable>> = build_flow_vars(&mut vars, &drainers, arcs, &inputs.pqc_edges);
        let mut constraints: Vec<Constraint> =
            Vec::with_capacity(drainers.len() * node_set.len() + undirected.len());
        for (k_idx, c) in drainers.iter().enumerate() {
            let src = inputs.dkms_to_qkc.get(c.src_dkms.as_str()).unwrap();
            let dst = inputs.dkms_to_qkc.get(c.dst_dkms.as_str()).unwrap();
            let delta_k = c.drain_rate;
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
                // Conservación con la fuente escalada por t:
                //   src: out − in = t·δ_k      dst: out − in = −t·δ_k
                let c_node = if node == src {
                    (expr - t * delta_k).eq(0.0)
                } else if node == dst {
                    (expr + t * delta_k).eq(0.0)
                } else {
                    expr.eq(0.0)
                };
                constraints.push(c_node);
            }
        }
        for ((a, b), cap) in undirected {
            let ij = arc_idx[&(a.clone(), b.clone())];
            let ji = arc_idx[&(b.clone(), a.clone())];
            let mut expr = Expression::with_capacity(2 * drainers.len());
            for row in x.iter().take(drainers.len()) {
                expr += row[ij];
                expr += row[ji];
            }
            constraints.push(expr.leq(*cap));
        }
        match solve_lp_backend(vars, t.into(), constraints, &[t]) {
            Ok(vals) => Some(vals[0].clamp(0.0, 1.0)),
            Err(e) => {
                warn!(error = %e, "fase 0 (max t) falló; fase 1 corre sin suelo max-min");
                None
            }
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
        t_floor: f64,
    ) -> Option<(f64, Vec<f64>, EdgeLoads)> {
        let mut vars = ProblemVariables::new();
        let lambda = vars.add(variable().min(0.0));
        let x: Vec<Vec<Variable>> = build_flow_vars(&mut vars, active, arcs, &inputs.pqc_edges);
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
        // Con el suelo max-min de la fase 0, σ_k ≤ (1−t*)·δ_k: la fase 1
        // sigue minimizando Σσ, pero ya no puede concentrar la denegación en
        // una commodity por debajo de la fracción común.
        let sigma: Vec<Variable> = active
            .iter()
            .map(|c| {
                let upper = if slack_disabled {
                    0.0
                } else {
                    c.drain_rate.max(0.0) * (1.0 - t_floor).max(0.0)
                };
                vars.add(variable().min(0.0).max(upper))
            })
            .collect();
        // Build objective: λ − M·Σ σ_k.
        let mut obj = Expression::with_capacity(1 + sigma.len());
        obj += lambda;
        for s in &sigma {
            obj += -LAMBDA_SLACK_PENALTY * *s;
        }
        let mut constraints: Vec<Constraint> =
            Vec::with_capacity(active.len() * node_set.len() + undirected.len());
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
                constraints.push(c_node);
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
            constraints.push(expr.leq(*cap));
        }
        // Carga por arista: sólo bajo `SDN_LOG_LP_DIAG=1`. Recuperarla exige
        // pedir las 2·|K| variables de flujo de cada arista —con 90
        // commodities y 14 aristas son 2520 valores— y esto corre cada
        // `mcf_period_ms`. Barato comparado con el LP, pero no gratis, y sólo
        // hace falta cuando se está diagnosticando.
        let diag = lp_diag_enabled();
        let mut wanted = Vec::with_capacity(1 + sigma.len());
        wanted.push(lambda);
        wanted.extend_from_slice(&sigma);
        let mut edge_order: Vec<((String, String), f64)> = Vec::new();
        if diag {
            for ((a, b), cap) in undirected {
                let ij = arc_idx[&(a.clone(), b.clone())];
                let ji = arc_idx[&(b.clone(), a.clone())];
                for row in x.iter().take(active.len()) {
                    wanted.push(row[ij]);
                    wanted.push(row[ji]);
                }
                edge_order.push(((a.clone(), b.clone()), *cap));
            }
        }
        match solve_lp_backend(vars, obj, constraints, &wanted) {
            Ok(vals) => {
                let lambda_v = vals[0].max(0.0);
                let n_sigma = sigma.len();
                let sigmas: Vec<f64> = vals[1..=n_sigma].iter().map(|v| v.max(0.0)).collect();
                let mut loads: EdgeLoads = Vec::new();
                if diag {
                    let per_edge = 2 * active.len();
                    let base = 1 + n_sigma;
                    for (i, (key, cap)) in edge_order.into_iter().enumerate() {
                        let from = base + i * per_edge;
                        let sum: f64 = vals[from..from + per_edge].iter().map(|v| v.max(0.0)).sum();
                        loads.push((key, sum, cap));
                    }
                }
                Some((lambda_v, sigmas, loads))
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
        // iter-009: λ is NOT a variable in phase-2 — substitute literally
        // by `lambda_star` (the phase-1 optimum). The original formulation
        // had `lambda` as a free variable with constraint `λ ≥ λ* − slack`,
        // which left a degree of freedom that microlp filled with numerical
        // drift, eventually breaking the conservation constraints and
        // reporting false-Infeasible in ~97% of solves. Pinning λ removes
        // the drift source entirely: the LP has only x (flow) and η (lex)
        // variables now.
        let mut vars = ProblemVariables::new();
        let x: Vec<Vec<Variable>> = build_flow_vars(&mut vars, active, arcs, &inputs.pqc_edges);
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
        let mut constraints: Vec<Constraint> =
            Vec::with_capacity(active.len() * node_set.len() + undirected.len());
        // Conservation with η_k present at source/sink, λ pinned to λ*.
        for (k_idx, c) in active.iter().enumerate() {
            let src = inputs.dkms_to_qkc.get(c.src_dkms.as_str()).unwrap();
            let dst = inputs.dkms_to_qkc.get(c.dst_dkms.as_str()).unwrap();
            let r_k = c.remaining();
            let delta_k = c.drain_rate;
            let eta_k = eta[k_idx];
            let lambda_r_k = lambda_star * r_k; // constante, no Variable
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
                // Substituyendo λ → λ*:
                //   src: (out - in) - λ*·R_k - η_k = δ_k
                //        → (out - in - η_k) = δ_k + λ*·R_k
                //   dst: (out - in) + λ*·R_k + η_k = -δ_k
                //        → (out - in + η_k) = -δ_k - λ*·R_k
                let c_node = if node == src {
                    (expr - eta_k).eq(delta_k + lambda_r_k)
                } else if node == dst {
                    (expr + eta_k).eq(-delta_k - lambda_r_k)
                } else {
                    expr.eq(0.0)
                };
                constraints.push(c_node);
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
            constraints.push(expr.leq(*cap));
        }
        // Wanted values: η_k first, then the full x matrix row-major —
        // phase 2's flow assignment is the final routing, so every
        // x^k_{ij} is read back to build the edge-flow support.
        let n_arcs = arcs.len();
        let mut wanted = Vec::with_capacity(eta.len() + active.len() * n_arcs);
        wanted.extend_from_slice(&eta);
        for row in &x {
            wanted.extend_from_slice(row);
        }
        match solve_lp_backend(vars, obj, constraints, &wanted) {
            Ok(vals) => {
                let eta_values: Vec<f64> = vals[..eta.len()].iter().map(|v| v.max(0.0)).collect();
                let mut edge_flows: Vec<EdgeFlow> = Vec::new();
                for (k_idx, c) in active.iter().enumerate() {
                    let fid = flow_id(&c.src_dkms, &c.dst_dkms);
                    let base = eta.len() + k_idx * n_arcs;
                    for (a_idx, (a, b)) in arcs.iter().enumerate() {
                        let v = vals[base + a_idx];
                        if v > FLOW_EPSILON {
                            edge_flows.push(EdgeFlow {
                                flow_id: fid.clone(),
                                src_qkc: a.clone(),
                                dst_qkc: b.clone(),
                                flow: v,
                                grade: c.grade,
                            });
                        }
                    }
                }
                Phase2Output {
                    eta_values,
                    edge_flows,
                    fallback: false,
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
                    fallback: true,
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
    /// `true` sólo cuando la fase 2 se INTENTÓ y el LP falló (la falsa
    /// infeasibilidad de microlp): distingue "η=0 porque el backend lo
    /// decidió" de "η=0 porque el solve se cayó", que en las muestras de
    /// `/rate` se ven idénticos y en CESGA hubo que separarlos a mano.
    fallback: bool,
}

/// Build the safe fallback solution: `λ = 0`, `r_k = 0` per
/// commodity, no edge flows. Used when an LP solve fails — every
/// commodity still gets a `rates` entry so DKMSs polling `/rate`
/// don't see an empty `peers` map and conclude "DKMS unknown".
fn zero_rate_fallback(active: &[&CommodityDemand]) -> McmcfSolution {
    let mut rates: HashMap<String, f64> = HashMap::with_capacity(active.len());
    let mut rates_grade: HashMap<(String, KeyGrade), f64> = HashMap::with_capacity(active.len());
    for c in active {
        let fid = flow_id(&c.src_dkms, &c.dst_dkms);
        rates.entry(fid.clone()).or_insert(0.0);
        rates_grade.insert((fid, c.grade), 0.0);
    }
    McmcfSolution {
        lambda: 0.0,
        edge_flows: Vec::new(),
        rates,
        rates_grade,
    }
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Dkms, EdgeMeta, HostEndpoint, LinkType, Orr, Qkc, Topology};

    /// Phase 2 (lex refinement / η / edge_flows → WCMP) is forced off
    /// on the Clarabel backend — interior-point solutions put
    /// real-magnitude circulation flow on the degenerate phase-2 LP,
    /// which would corrupt the WCMP tables (see the comment at the
    /// `lex_disabled` computation in `solve`). Tests asserting phase-2
    /// behaviour are therefore skipped under `SDN_SOLVER=clarabel`;
    /// they still run on the default highs backend (and on microlp).
    fn phase2_unavailable() -> bool {
        lp_backend() == LpBackend::Clarabel
    }

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
                peer_addr: None,
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
            peer_addr: None,
            tls_id: None,
            orr_id: format!("o-{qa}"),
        });
        t.upsert_dkms(Dkms {
            id: db.into(),
            host: host(21),
            peer_addr: None,
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
                ..Default::default()
            },
        );
    }

    fn pqc_link(t: &mut Topology, a: &str, b: &str) {
        t.add_edge(
            a,
            b,
            EdgeMeta {
                link_type: LinkType::Pqc,
                ..Default::default()
            },
        );
    }

    /// A PQC edge enters the MCF with the unbounded sentinel capacity.
    #[test]
    fn pqc_edge_gets_declared_capacity() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        pqc_link(&mut t, "1", "2");
        let inputs = McmcfInputs::build(&t, &DemandRegistry::new());
        let cap = inputs
            .edge_capacity
            .get(&("1".to_string(), "2".to_string()))
            .copied()
            .expect("PQC edge present in edge_capacity");
        // El default del campo declarado, no un centinela: 10 000 claves/s.
        assert_eq!(cap, 10_000.0);
    }

    /// A PQC link outperforms a slow QKD link but is BOUNDED by its declared
    /// capacity (default 10 000). This replaces the old "PQC removes the
    /// bottleneck" semantics: the 1e9 sentinel made λ and every /rate value
    /// meaningless in PQC-only deployments (measured 2026-08-02); a declared
    /// finite capacity is what gives the rate signal meaning there.
    #[test]
    fn pqc_edge_bounded_by_declared_capacity() {
        let solve = |pqc: bool| {
            let mut t = Topology::default();
            add_dkms_pair(&mut t, "dA", "dB", "1", "2");
            if pqc {
                pqc_link(&mut t, "1", "2");
            } else {
                link(&mut t, "1", "2", 100.0);
            }
            McmcfSolver::new().solve(&McmcfInputs::build(&t, &DemandRegistry::new()))
        };
        let r_qkd = solve(false).rates[&flow_id("dA", "dB")];
        let r_pqc = solve(true).rates[&flow_id("dA", "dB")];
        assert!(r_qkd > 0.0, "QKD rate should be positive, got {r_qkd}");
        assert!(
            r_pqc > 100.0,
            "PQC rate {r_pqc} must exceed the QKD edge's finite bound (~50/dir)"
        );
        // Dos commodities (ida y vuelta) comparten los 10 000 del enlace: la
        // rate por sentido queda en torno a 5 000 y NUNCA por encima de la
        // capacidad declarada — antes, con el centinela, salía ~5e8.
        assert!(
            r_pqc <= 10_000.0 + 1.0,
            "PQC rate {r_pqc} must be bounded by the declared capacity"
        );
    }

    /// Per-grade arc eligibility: a QKD-grade commodity must NOT use a PQC
    /// shortcut even when one exists. Topology `1=2 (QKD,1000) 2=3 (QKD,1000)
    /// 1~3 (PQC,1e9)`, commodity d1→d3 (R_k=4096, δ=0):
    ///   * QKD-grade → may only route 1-2-3 ⇒ λ bounded by the QKD min-cut
    ///     1000/4096 ≈ 0.244 (the PQC arc's flow vars are pinned to 0).
    ///   * PQC-grade → may take the 1e9 shortcut ⇒ λ explodes.
    #[test]
    fn build_splits_pair_into_two_grade_commodities() {
        use crate::demand::DemandReport;
        // QKD-connected pair dA-dB (QKD link) with demand reported at BOTH
        // grades (e.g. a strict_qkd SAE and a no_worry SAE) → two commodities.
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0); // QKD link → pair is QKD-connected
        let reg = DemandRegistry::new();
        let mk = |grade, drain| CommodityDemand {
            src_dkms: "dA".into(),
            dst_dkms: "dB".into(),
            level: 0.0,
            capacity: 4096.0,
            drain_rate: drain,
            timestamp_ms: 1,
            grade,
        };
        reg.ingest(DemandReport {
            dkms_id: "dA".into(),
            entries: vec![mk(KeyGrade::Qkd, 10.0), mk(KeyGrade::Pqc, 20.0)],
        });
        let inputs = McmcfInputs::build(&t, &reg);
        let dab: Vec<_> = inputs
            .commodities
            .iter()
            .filter(|c| c.src_dkms == "dA" && c.dst_dkms == "dB")
            .collect();
        assert_eq!(dab.len(), 2, "both-grade demand → two commodities");
        assert!(dab
            .iter()
            .any(|c| c.grade == KeyGrade::Qkd && (c.drain_rate - 10.0).abs() < 1e-9));
        assert!(dab
            .iter()
            .any(|c| c.grade == KeyGrade::Pqc && (c.drain_rate - 20.0).abs() < 1e-9));
    }

    #[test]
    fn qkd_grade_commodity_cannot_use_pqc_shortcut() {
        let mut edge_capacity: HashMap<(String, String), f64> = HashMap::new();
        edge_capacity.insert(("1".into(), "2".into()), 1000.0);
        edge_capacity.insert(("2".into(), "3".into()), 1000.0);
        edge_capacity.insert(("1".into(), "3".into()), 1e9); // PQC shortcut
        let pqc_edges: HashSet<(String, String)> =
            [("1".to_string(), "3".to_string())].into_iter().collect();
        let dkms_to_qkc: HashMap<String, String> = [
            ("d1".to_string(), "1".to_string()),
            ("d3".to_string(), "3".to_string()),
        ]
        .into_iter()
        .collect();
        let commodity = |grade| CommodityDemand {
            src_dkms: "d1".into(),
            dst_dkms: "d3".into(),
            level: 0.0,
            capacity: 4096.0,
            drain_rate: 0.0,
            timestamp_ms: 0,
            grade,
        };
        let inputs = |grade| McmcfInputs {
            commodities: vec![commodity(grade)],
            edge_capacity: edge_capacity.clone(),
            dkms_to_qkc: dkms_to_qkc.clone(),
            pqc_edges: pqc_edges.clone(),
        };

        let lam_qkd = McmcfSolver::new().solve(&inputs(KeyGrade::Qkd)).lambda;
        let lam_pqc = McmcfSolver::new().solve(&inputs(KeyGrade::Pqc)).lambda;

        // QKD-grade is pinned off the PQC shortcut → bounded by the QKD path.
        let qkd_bound = 1000.0 / 4096.0; // ≈ 0.244
        assert!(
            lam_qkd > 0.0 && lam_qkd < qkd_bound * 1.1,
            "QKD-grade λ={lam_qkd} should be ≈ QKD min-cut {qkd_bound}, not the PQC shortcut"
        );
        // PQC-grade rides the 1e9 shortcut → vastly larger λ.
        assert!(
            lam_pqc > lam_qkd * 100.0,
            "PQC-grade λ={lam_pqc} should dwarf QKD-grade λ={lam_qkd}"
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
                grade: Default::default(),
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
            peer_addr: None,
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
            peer_addr: None,
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(22),
            peer_addr: None,
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

    /// El suelo max-min de la fase 0: bajo sobrecarga (Σδ=400 sobre una
    /// arista de 100), NINGUNA commodity queda por debajo de su fracción
    /// común t* = 100/400 = 0.25. Sin la fase 0, `max λ − M·Σσ` es
    /// indiferente entre repartir y concentrar, y el vértice del símplex
    /// podía dejar una de las dos a cero (medido en CESGA 2026-08-25:
    /// pares fijos al 100 % de ceros durante toda la carga).
    #[test]
    fn lp_floor_prevents_vertex_starvation() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        link(&mut t, "1", "2", 100.0);
        let reg = DemandRegistry::new();
        report(&reg, "dA", "dB", 2000.0, 4096.0, 300.0);
        report(&reg, "dB", "dA", 2000.0, 4096.0, 100.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        let r_ab = sol.rates[&flow_id("dA", "dB")];
        let r_ba = sol.rates[&flow_id("dB", "dA")];
        // Suelos: 0.25·300 = 75 y 0.25·100 = 25 (menos tolerancia numérica).
        assert!(r_ab >= 74.0, "dA→dB por debajo de su suelo max-min: {r_ab}");
        assert!(r_ba >= 24.0, "dB→dA por debajo de su suelo max-min: {r_ba}");
        // Y la capacidad compartida se respeta.
        assert!(r_ab + r_ba <= 100.5, "capacidad violada: {r_ab}+{r_ba}");
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
            peer_addr: None,
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
            rates_grade: HashMap::new(),
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
                peer_addr: None,
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
                peer_addr: None,
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
        if phase2_unavailable() {
            common::test_support::skip_or_fail(
                "la fase 2 del LP está apagada con SDN_SOLVER=clarabel",
            );
            return;
        }
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
                peer_addr: None,
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
            peer_addr: None,
            tls_id: None,
            orr_id: "o-1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(202),
            peer_addr: None,
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
        if phase2_unavailable() {
            common::test_support::skip_or_fail(
                "la fase 2 del LP está apagada con SDN_SOLVER=clarabel",
            );
            return;
        }
        let t = diamond_topology();
        let reg = DemandRegistry::new();
        // dA→dB: empty buffer, high demand. dB→dA: full, no fill needed.
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let inputs = McmcfInputs::build(&t, &reg);
        let sol = McmcfSolver::new().solve(&inputs);
        assert!(sol.lambda > 0.0);

        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t, None);
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
        if phase2_unavailable() {
            common::test_support::skip_or_fail(
                "la fase 2 del LP está apagada con SDN_SOLVER=clarabel",
            );
            return;
        }
        let t = diamond_topology();
        let reg = DemandRegistry::new();
        report(&reg, "dA", "dB", 0.0, 4096.0, 0.0);
        report(&reg, "dB", "dA", 4096.0, 4096.0, 0.0);
        let sol = McmcfSolver::new().solve(&McmcfInputs::build(&t, &reg));
        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t, None);
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
            grade: KeyGrade::Qkd,
        }];
        let wcmp = wcmp_from_edge_flows(&flows, &t, None);
        let hops = &wcmp["1"]["2"];
        assert_eq!(hops.len(), 1);
        assert_eq!(hops[0].qkc_id, 11);
        assert!(
            hops[0].weight >= 1,
            "weight must not be 0, got {}",
            hops[0].weight
        );
    }

    /// El otro extremo del mismo cuantizador. Con las aristas PQC a capacidad
    /// centinela los flujos del LP se disparan; por encima de ~42,9 M claves/s
    /// el `flow × 100` ya no cabe en un u32 y el `as u32` daba la vuelta,
    /// convirtiendo un reparto en un resto de división. Debe topar arriba.
    #[test]
    fn huge_flow_saturates_instead_of_wrapping() {
        let t = diamond_topology();
        let weight_for = |flow: f64| {
            let flows = vec![EdgeFlow {
                flow_id: flow_id("dA", "dB"),
                src_qkc: "1".into(),
                dst_qkc: "11".into(),
                flow,
                grade: KeyGrade::Qkd,
            }];
            wcmp_from_edge_flows(&flows, &t, None)["1"]["2"][0].weight
        };

        // Por debajo del límite el peso sigue siendo el reparto exacto: este
        // es el valor real observado en el testbed el 2026-08-02.
        assert_eq!(weight_for(2.755_359_744e7), 2_755_359_744);

        // Por encima, satura en vez de envolver.
        for flow in [1e9_f64, 1e12, f64::MAX] {
            let w = weight_for(flow);
            assert_eq!(w, u32::MAX, "un flujo de {flow} debe saturar (dio {w})");
        }
    }

    /// `into_mcf_snapshot` exposes the WCMP table on the published
    /// snapshot so the forwarding push loop can read it.
    #[test]
    fn into_mcf_snapshot_includes_wcmp_table() {
        // La tabla sale de la topología, así que existe aunque el solver haya
        // devuelto la solución degenerada (cortocircuito de buffers llenos,
        // LP infeasible…). Esa independencia es el punto: era exactamente el
        // caso en que la red entera se quedaba sin multipath.
        let t = diamond_topology();
        let sol = McmcfSolution {
            lambda: 0.0,
            edge_flows: vec![], // lo que devuelven cortocircuito y fallback
            rates: HashMap::new(),
            rates_grade: HashMap::new(),
        };
        let snap = sol.into_mcf_snapshot(&t);
        assert!(!snap.wcmp.is_empty(), "wcmp must exist without the LP");
        let hops = &snap.wcmp["1"]["2"];
        assert_eq!(hops.len(), 2, "diamond: two downhill next hops");
    }

    /// El rombo: de 1 a 2 hay dos caminos de dos saltos (vía 11 y vía 22).
    /// Ambos vecinos están estrictamente más cerca del destino, así que el
    /// reparto usa los dos, y con capacidades iguales pesa igual.
    #[test]
    fn topology_wcmp_splits_across_strictly_closer_neighbours() {
        let t = diamond_topology();
        let w = wcmp_from_topology(&t, None);

        let hops = &w["1"]["2"];
        assert_eq!(
            hops.iter().map(|h| h.qkc_id).collect::<Vec<_>>(),
            vec![11, 22],
            "los dos caminos del rombo, en orden determinista",
        );
        assert_eq!(hops[0].weight, hops[1].weight, "capacidades iguales");

        // Destino adyacente: un único next hop, el propio destino.
        assert_eq!(w["1"]["11"].len(), 1);
        assert_eq!(w["1"]["11"][0].qkc_id, 11);

        // Y desde un lateral hacia el otro: sus dos vecinos (1 y 2) están a
        // distancia 1 del destino 22, así que también reparte.
        assert_eq!(
            w["11"]["22"].iter().map(|h| h.qkc_id).collect::<Vec<_>>(),
            vec![1, 2],
        );
    }

    /// Solo se encamina hacia vecinos ESTRICTAMENTE más cercanos al destino:
    /// la distancia decrece en cada salto, así que no puede haber bucles
    /// aunque cada QKC decida por su cuenta. Se verifica contra un BFS
    /// independiente del de producción.
    #[test]
    fn topology_wcmp_next_hops_are_strictly_closer_to_the_destination() {
        let t = diamond_topology();
        let w = wcmp_from_topology(&t, None);

        let dist = |from: &str, to: &str| -> u32 {
            let mut d = HashMap::from([(from.to_string(), 0u32)]);
            let mut q = std::collections::VecDeque::from([from.to_string()]);
            while let Some(v) = q.pop_front() {
                for n in t.graph.get(&v).into_iter().flatten() {
                    if !d.contains_key(n) {
                        d.insert(n.clone(), d[&v] + 1);
                        q.push_back(n.clone());
                    }
                }
            }
            d[to]
        };

        for (src, by_dst) in &w {
            for (dst, hops) in by_dst {
                for h in hops {
                    let nh = h.qkc_id.to_string();
                    assert!(
                        dist(&nh, dst) < dist(src, dst),
                        "{src}→{dst} vía {nh}: el next hop no acerca",
                    );
                }
            }
        }
    }

    /// El peso de cada next hop es el cuello de botella del mejor camino
    /// descendente que pasa por él — no la capacidad del primer salto.
    #[test]
    fn topology_wcmp_weights_follow_the_path_bottleneck() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        for q in ["11", "22"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                peer_addr: None,
                kme_host: None,
            });
        }
        // Vía 11: primer salto ancho (100) pero cuello 10 después.
        link(&mut t, "1", "11", 100.0);
        link(&mut t, "11", "2", 10.0);
        // Vía 22: 50 sostenido.
        link(&mut t, "1", "22", 50.0);
        link(&mut t, "22", "2", 50.0);

        let w = wcmp_from_topology(&t, None);
        let hops = &w["1"]["2"];
        let weight_of = |id: u32| hops.iter().find(|h| h.qkc_id == id).unwrap().weight;
        // α=0.2, d=0 ⇒ capacidad = r0. Cuantización ×100.
        assert_eq!(weight_of(11), 10 * 100, "min(100, 10), no 100");
        assert_eq!(weight_of(22), 50 * 100);
    }

    /// La tabla QKD-only encamina por el subgrafo QKD: un frame de ese grado
    /// jamás debe pisar un enlace PQC, aunque el camino PQC sea más corto.
    #[test]
    fn topology_wcmp_qkd_table_ignores_pqc_edges() {
        let mut t = Topology::default();
        add_dkms_pair(&mut t, "dA", "dB", "1", "2");
        t.upsert_qkc(Qkc {
            id: "3".into(),
            host: host(3),
            peer_addr: None,
            kme_host: None,
        });
        // Directo 1–2 por PQC; rodeo 1–3–2 por QKD.
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                link_type: LinkType::Pqc,
                ..Default::default()
            },
        );
        link(&mut t, "1", "3", 100.0);
        link(&mut t, "3", "2", 100.0);

        let full = wcmp_from_topology(&t, None);
        let qkd = wcmp_from_topology(&t, Some(KeyGrade::Qkd));

        // La tabla completa va por el directo PQC (un salto).
        assert_eq!(
            full["1"]["2"].iter().map(|h| h.qkc_id).collect::<Vec<_>>(),
            vec![2]
        );
        // La QKD-only lo ignora y rodea por 3.
        assert_eq!(
            qkd["1"]["2"].iter().map(|h| h.qkc_id).collect::<Vec<_>>(),
            vec![3]
        );
    }

    /// Mismo snapshot ⇒ misma tabla, byte a byte: las tablas se re-POSTean a
    /// los QKC y una ordenación inestable parecería un cambio de ruta en cada
    /// push.
    #[test]
    fn topology_wcmp_is_deterministic() {
        let t = diamond_topology();
        assert_eq!(wcmp_from_topology(&t, None), wcmp_from_topology(&t, None));
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
                peer_addr: None,
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
                peer_addr: None,
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
                let id = format!("{row}{col}");
                let host_id = (row * 4 + col + 1) as i64;
                t.upsert_qkc(Qkc {
                    id: id.clone(),
                    host: host(host_id),
                    peer_addr: None,
                    kme_host: None,
                });
            }
        }
        // Horizontal edges (within each row).
        for row in 0..3 {
            for col in 0..3 {
                let a = format!("{row}{col}");
                let b = format!("{}{}", row, col + 1);
                link(&mut t, &a, &b, 1_000.0);
            }
        }
        // Vertical edges (between adjacent rows).
        for row in 0..2 {
            for col in 0..4 {
                let a = format!("{row}{col}");
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
                peer_addr: None,
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
        if phase2_unavailable() {
            common::test_support::skip_or_fail(
                "la fase 2 del LP está apagada con SDN_SOLVER=clarabel",
            );
            return;
        }
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
        let wcmp = wcmp_from_edge_flows(&sol.edge_flows, &t, None);
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
            "expected fan-out via both 01 and 10, got {hop_ids:?}"
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

    /// Same trivial LP, pinned to the Clarabel backend regardless of
    /// `SDN_SOLVER` — proves the alternative backend is compiled in
    /// and solves. Interior-point values are approximate, hence the
    /// looser tolerance than the microlp smoke test.
    #[test]
    fn clarabel_smoke_solves_trivial_problem() {
        use good_lp::constraint;
        let mut vars = good_lp::ProblemVariables::new();
        let x = vars.add(variable().min(0.0).max(1.0));
        let solution = vars
            .maximise(x)
            .using(good_lp::clarabel)
            .with(constraint!(x <= 1.0))
            .solve()
            .expect("trivial LP must solve on clarabel");
        let xv = solution.value(x);
        assert!((xv - 1.0).abs() < 1e-4, "expected x ≈ 1.0, got {xv}");
    }

    /// El backend por defecto desde 2026-08-25. Prueba que HiGHS está
    /// compilado y resuelve — si este test no linka, falta `cmake` en el
    /// entorno de build (Dockerfiles y cesga.sbatch ya lo llevan).
    #[test]
    fn highs_smoke_solves_trivial_problem() {
        use good_lp::constraint;
        let mut vars = good_lp::ProblemVariables::new();
        let x = vars.add(variable().min(0.0).max(1.0));
        let solution = vars
            .maximise(x)
            .using(good_lp::highs)
            .with(constraint!(x <= 1.0))
            .solve()
            .expect("trivial LP must solve on highs");
        let xv = solution.value(x);
        assert!((xv - 1.0).abs() < 1e-4, "expected x ≈ 1.0, got {xv}");
    }
}
