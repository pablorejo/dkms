//! Min-Cost-Flow solver for the SDN — strict-priority + round-robin
//! (water-filling) within each priority class.
//!
//! This is a direct port of the productive path of the Python solver
//! (`code_dkms/src/SDN/mcf.py::_solve_strict_priority_within_class`,
//! plus `_solve_proportional_fair`'s water-filling). The legacy
//! `scipy.optimize.linprog`-based lex-max-min solver is intentionally
//! omitted — the Python branch we mirror uses SP+RR exclusively.
//!
//! Algorithm sketch:
//!
//! 1. **Build commodities.** For every ordered DKMS pair `(s, d)` with
//!    distinct anchor QKCs, pre-compute up to `k_paths` shortest paths
//!    (Yen over BFS). Each commodity carries `src_qkc`, `dst_qkc` and
//!    its candidate paths.
//! 2. **Group by priority.** Each flow gets a weight from the caller
//!    (default 1.0); higher weight = higher class.
//! 3. **For each class, descending weight:** run proportional-fair
//!    water-filling over the *remaining* edge capacities. The flows in
//!    that class share capacity fairly; flows in lower classes only
//!    see what's left over.
//! 4. **Subtract used capacity** along the shortest path of each flow
//!    that got a non-zero rate, then move to the next class.
//!
//! Water-filling fixed point:
//!
//! ```text
//!   r_i = w_i / Σ_{e ∈ p_i} λ_e
//!   λ_e ← λ_e · exp(η · (usage_e − C_e) / C_e)
//! ```
//!
//! Converges in ~50–200 iterations for the sizes we care about. Pure
//! arithmetic over `Vec<f64>` — no extra deps required.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use tracing::debug;

use crate::topology::{edge_key, EdgeKey, Topology};

// Re-export so call sites can keep using `mcf::BufferRole` as the
// solver-facing name while the canonical definition lives next to the
// rest of the priority machinery.
pub use crate::priority::BufferRole;

// ---------------------------------------------------------------- types

/// A path in the QKC graph: ordered sequence of QKC ids.
pub type Path = Vec<String>;

/// Build the canonical id used to key a flow `(src_dkms → dst_dkms)`.
pub fn flow_id(src_dkms: &str, dst_dkms: &str) -> String {
    format!("{src_dkms}->{dst_dkms}")
}

/// One MCF commodity = an ordered DKMS pair plus its candidate paths
/// (already projected onto QKCs).
#[derive(Debug, Clone)]
pub struct Commodity {
    pub src_dkms: String,
    pub dst_dkms: String,
    pub src_qkc: String,
    pub dst_qkc: String,
    pub paths: Vec<Path>,
}

impl Commodity {
    pub fn flow_id(&self) -> String {
        flow_id(&self.src_dkms, &self.dst_dkms)
    }
}

/// Result of a solve: per-flow rate, forwarding table per QKC, and a
/// per-DKMS buffer view.
#[derive(Debug, Default, Clone)]
pub struct McfSnapshot {
    /// `flow_id → r_f` (keys/s). Only flows with positive rate appear.
    pub rates: HashMap<String, f64>,

    /// `qkc_id → flow_id → [(next_hop_qkc, omega)]`. With single-path
    /// solving `omega = 1.0`; the structure is kept multi-path-ready.
    pub forwarding: HashMap<String, HashMap<String, Vec<(String, f64)>>>,

    /// `dkms_id → (peer_dkms, role) → r_f`. For a flow `A → B` with
    /// rate `r`:
    ///
    /// * `rates_by_dkms[A][(B, Enc)] = r` — what A pushes to B.
    /// * `rates_by_dkms[B][(A, Dec)] = r` — what B receives from A.
    ///
    /// Flows `A → B` and `B → A` are independent and may differ.
    pub rates_by_dkms: HashMap<String, HashMap<(String, BufferRole), f64>>,
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

// ---------------------------------------------------------------- K-shortest

/// Up to `k` simple shortest paths from `src` to `dst` in `graph`, in
/// non-decreasing number-of-hops order. Variant of Yen using BFS for
/// the shortest-path subroutine, matching the Python implementation.
pub fn k_shortest_paths(
    graph: &HashMap<String, HashSet<String>>,
    src: &str,
    dst: &str,
    k: usize,
) -> Vec<Path> {
    if src == dst || !graph.contains_key(src) || !graph.contains_key(dst) || k == 0 {
        return vec![];
    }

    let first = match bfs_shortest(graph, &HashSet::new(), &HashSet::new(), src, dst) {
        Some(p) => p,
        None => return vec![],
    };
    let mut shortest = vec![first];

    // Candidate heap, ordered by (path_len, insertion_counter) — we
    // want shortest first, with stable tie-breaking like the Python
    // version's heapq.
    let mut candidates: BTreeMap<(usize, usize), Path> = BTreeMap::new();
    let mut counter: usize = 0;

    while shortest.len() < k {
        let prev_path = shortest.last().unwrap().clone();
        for i in 0..prev_path.len().saturating_sub(1) {
            let spur_node = &prev_path[i];
            let root_path = &prev_path[..=i];

            let mut excluded_edges: HashSet<EdgeKey> = HashSet::new();
            let mut excluded_nodes: HashSet<String> = HashSet::new();
            for n in &root_path[..root_path.len() - 1] {
                excluded_nodes.insert(n.clone());
            }
            for p in &shortest {
                if p.len() > i && &p[..=i] == root_path {
                    excluded_edges.insert(edge_key(&p[i], &p[i + 1]));
                }
            }

            let Some(spur_path) =
                bfs_shortest(graph, &excluded_edges, &excluded_nodes, spur_node, dst)
            else {
                continue;
            };

            let mut full_path: Path = root_path[..root_path.len() - 1].to_vec();
            full_path.extend(spur_path);

            if shortest.iter().any(|p| p == &full_path)
                || candidates.values().any(|p| p == &full_path)
            {
                continue;
            }
            candidates.insert((full_path.len(), counter), full_path);
            counter += 1;
        }
        let Some((&key, _)) = candidates.iter().next() else {
            break;
        };
        let next_path = candidates.remove(&key).unwrap();
        shortest.push(next_path);
    }

    shortest
}

fn bfs_shortest(
    graph: &HashMap<String, HashSet<String>>,
    excluded_edges: &HashSet<EdgeKey>,
    excluded_nodes: &HashSet<String>,
    start: &str,
    dst: &str,
) -> Option<Path> {
    if start == dst {
        return Some(vec![start.to_string()]);
    }
    let mut prev: HashMap<String, Option<String>> = HashMap::new();
    prev.insert(start.into(), None);
    let mut queue: VecDeque<String> = VecDeque::from([start.to_string()]);
    while let Some(node) = queue.pop_front() {
        let mut neighbors: Vec<&String> = graph
            .get(&node)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        neighbors.sort();
        for n in neighbors {
            if prev.contains_key(n) {
                continue;
            }
            if excluded_nodes.contains(n) && n != dst {
                continue;
            }
            if excluded_edges.contains(&edge_key(&node, n)) {
                continue;
            }
            prev.insert(n.clone(), Some(node.clone()));
            if n == dst {
                let mut path = vec![dst.to_string()];
                let mut cur = dst.to_string();
                while let Some(Some(p)) = prev.get(&cur).cloned() {
                    path.push(p.clone());
                    cur = p;
                }
                path.reverse();
                return Some(path);
            }
            queue.push_back(n.clone());
        }
    }
    None
}

fn path_edges(path: &[String]) -> Vec<EdgeKey> {
    path.windows(2).map(|w| edge_key(&w[0], &w[1])).collect()
}

// ---------------------------------------------------------------- Solver

/// SDN MCF solver. Stateless aside from the K-paths configuration.
#[derive(Debug, Clone, Copy)]
pub struct McfSolver {
    pub k_paths: usize,
}

impl Default for McfSolver {
    fn default() -> Self {
        Self { k_paths: 3 }
    }
}

impl McfSolver {
    pub fn new(k_paths: usize) -> Self {
        Self {
            k_paths: k_paths.max(1),
        }
    }

    /// Generate the full commodity set from a topology snapshot. Pairs
    /// of DKMSs anchored to the same QKC are skipped (no physical link
    /// in between → no demand on the network).
    pub fn build_commodities(&self, topo: &Topology) -> Vec<Commodity> {
        let mut dkms_ids: Vec<&str> = topo.dkms.keys().map(String::as_str).collect();
        dkms_ids.sort();

        let mut out = Vec::with_capacity(dkms_ids.len() * dkms_ids.len().saturating_sub(1));
        for &s in &dkms_ids {
            for &d in &dkms_ids {
                if s == d {
                    continue;
                }
                let Some(src_qkc) = topo.qkc_of_dkms(s).map(str::to_string) else {
                    continue;
                };
                let Some(dst_qkc) = topo.qkc_of_dkms(d).map(str::to_string) else {
                    continue;
                };
                if src_qkc == dst_qkc {
                    continue;
                }
                let paths = k_shortest_paths(&topo.graph, &src_qkc, &dst_qkc, self.k_paths);
                if paths.is_empty() {
                    continue;
                }
                out.push(Commodity {
                    src_dkms: s.into(),
                    dst_dkms: d.into(),
                    src_qkc,
                    dst_qkc,
                    paths,
                });
            }
        }
        out
    }

    /// Produce the per-edge capacity map (keys/s) from a topology
    /// snapshot. Keys are canonical sorted pairs, matching the format
    /// the solver consumes.
    pub fn capacities(&self, topo: &Topology) -> HashMap<EdgeKey, f64> {
        topo.edges
            .iter()
            .map(|(k, m)| (k.clone(), m.quditto_capacity_keys_per_second()))
            .collect()
    }

    /// Run the solver. `weights` maps `flow_id → w_f`; missing flows
    /// default to `1.0`. Flows with weight `≤ 0` are excluded entirely
    /// (the caller is expected to override their rate to 0 outside).
    ///
    /// Strategy: **hybrid two-tier weighted max-min**.
    ///
    /// * **HIGH tier** (`w ≥ TIER_THRESHOLD`): the upper QoS classes
    ///   (Priority, Important, Quickly) compete for the full edge
    ///   capacity via weighted max-min — decade-spaced weights give
    ///   them a 10×/100× ratio without starving each other.
    /// * **LOW tier** (`0 < w < TIER_THRESHOLD`): the lower classes
    ///   (Relax, BestEffort) see only the **residual** capacity left
    ///   by the HIGH tier. Strict between tiers, max-min within.
    ///
    /// With the default class weights (Pri=10000, Imp=1000, Qkly=100,
    /// Relax=10, BE=1), `TIER_THRESHOLD = 100.0` puts the natural
    /// boundary between Quickly and Relax.
    pub fn solve(
        &self,
        commodities: &[Commodity],
        capacities: &HashMap<EdgeKey, f64>,
        weights: &HashMap<String, f64>,
    ) -> McfSnapshot {
        const TIER_THRESHOLD: f64 = 100.0;

        let weight_of = |c: &Commodity| weights.get(&c.flow_id()).copied().unwrap_or(1.0);
        let high: Vec<&Commodity> = commodities
            .iter()
            .filter(|c| weight_of(c) >= TIER_THRESHOLD)
            .collect();
        let low: Vec<&Commodity> = commodities
            .iter()
            .filter(|c| {
                let w = weight_of(c);
                w > 0.0 && w < TIER_THRESHOLD
            })
            .collect();

        // First pass: HIGH tier on the full capacity.
        let mut snap = McfSnapshot::default();
        let mut remaining = capacities.clone();
        if !high.is_empty() {
            let sub = self.weighted_maxmin(&high, &remaining, weights);
            self.subtract_usage(&mut remaining, &high, &sub);
            self.merge_into(&mut snap, sub);
        }
        // Second pass: LOW tier on what's left.
        if !low.is_empty() && remaining.values().any(|c| *c > 1e-9) {
            let sub = self.weighted_maxmin(&low, &remaining, weights);
            self.merge_into(&mut snap, sub);
        }
        debug!(
            high = high.len(),
            low = low.len(),
            flows_with_rate = snap.rates.len(),
            "hybrid solve done"
        );
        snap
    }

    /// Subtract flow rates from `remaining` along each commodity's
    /// first path. Defensive bound at 0 to guard against fp noise.
    fn subtract_usage(
        &self,
        remaining: &mut HashMap<EdgeKey, f64>,
        commodities: &[&Commodity],
        snap: &McfSnapshot,
    ) {
        for c in commodities {
            let r = snap.rates.get(&c.flow_id()).copied().unwrap_or(0.0);
            if r <= 0.0 {
                continue;
            }
            let Some(p) = c.paths.first() else { continue };
            for (u, v) in p.iter().zip(p.iter().skip(1)) {
                let k = edge_key(u, v);
                if let Some(cap) = remaining.get_mut(&k) {
                    *cap = (*cap - r).max(0.0);
                }
            }
        }
    }

    fn merge_into(&self, dst: &mut McfSnapshot, src: McfSnapshot) {
        for (fid, r) in src.rates {
            dst.rates.insert(fid, r);
        }
        for (dkms, m) in src.rates_by_dkms {
            dst.rates_by_dkms.entry(dkms).or_default().extend(m);
        }
        for (qkc, ft) in src.forwarding {
            let merged = dst.forwarding.entry(qkc).or_default();
            for (fid, entries) in ft {
                merged.insert(fid, entries);
            }
        }
    }

    // -------------------------------------------------------- Weighted max-min
    //
    // Progressive-filling algorithm — at each iteration grow every
    // active flow by `w_i × Δ` where Δ is the largest increment any
    // edge can absorb. The first edge to saturate freezes the flows
    // passing through it; the remaining flows keep growing on the
    // remaining capacity. Repeat until no flow is active.
    //
    // Properties (with decade-spaced class weights):
    // * Within a tier, Priority gets ≈10× Important's rate on a shared
    //   edge; no class is pinned to 0.
    // * Caller (`solve`) supplies the per-tier commodity subset; this
    //   routine doesn't know about tiers.
    // * O(F × E) per iteration, at most E iterations → O(F × E²).
    //   For the typical sim (12 flows × 6 edges) this is trivial.

    fn weighted_maxmin(
        &self,
        commodities: &[&Commodity],
        capacities: &HashMap<EdgeKey, f64>,
        weights: &HashMap<String, f64>,
    ) -> McfSnapshot {
        let mut snap = McfSnapshot::default();
        if commodities.is_empty() || capacities.is_empty() {
            return snap;
        }

        // Stable edge ordering.
        let edge_list: Vec<EdgeKey> = {
            let mut v: Vec<_> = capacities.keys().cloned().collect();
            v.sort();
            v
        };
        let edge_idx: HashMap<&EdgeKey, usize> =
            edge_list.iter().enumerate().map(|(i, k)| (k, i)).collect();
        let n_edges = edge_list.len();
        let n_flows = commodities.len();

        // Per-flow edges. Flows without a path are excluded.
        let mut flow_edges: Vec<Vec<usize>> = vec![vec![]; n_flows];
        for (i, c) in commodities.iter().enumerate() {
            let Some(p) = c.paths.first() else { continue };
            for ek in path_edges(p) {
                if let Some(&e) = edge_idx.get(&ek) {
                    flow_edges[i].push(e);
                }
            }
        }

        let flow_weights: Vec<f64> = commodities
            .iter()
            .map(|c| weights.get(&c.flow_id()).copied().unwrap_or(1.0))
            .collect();
        let caps: Vec<f64> = edge_list.iter().map(|k| capacities[k]).collect();

        let mut rates = vec![0.0_f64; n_flows];
        let mut remaining = caps.clone();
        // Active iff weight > 0 AND has at least one edge in the graph.
        let mut active: Vec<bool> = (0..n_flows)
            .map(|i| flow_weights[i] > 0.0 && !flow_edges[i].is_empty())
            .collect();

        const EPS: f64 = 1e-9;
        let mut guard = 0usize; // hard cap, in case of pathological input
        loop {
            guard += 1;
            if guard > n_edges + 2 {
                debug!("weighted_maxmin: guard tripped at iter {guard}");
                break;
            }
            // Sum of active flow weights traversing each edge.
            let mut edge_w: Vec<f64> = vec![0.0; n_edges];
            for i in 0..n_flows {
                if !active[i] {
                    continue;
                }
                for &e in &flow_edges[i] {
                    edge_w[e] += flow_weights[i];
                }
            }
            // Largest Δ that fits in every edge.
            let mut delta = f64::INFINITY;
            for e in 0..n_edges {
                if edge_w[e] > EPS && remaining[e] > EPS {
                    let d = remaining[e] / edge_w[e];
                    if d < delta {
                        delta = d;
                    }
                }
            }
            if !delta.is_finite() || delta <= EPS {
                break;
            }
            // Grow active flows.
            for i in 0..n_flows {
                if active[i] {
                    rates[i] += flow_weights[i] * delta;
                }
            }
            // Subtract consumed capacity.
            for e in 0..n_edges {
                if edge_w[e] > EPS {
                    remaining[e] -= edge_w[e] * delta;
                    if remaining[e] < EPS {
                        remaining[e] = 0.0;
                    }
                }
            }
            // Freeze flows through any saturated edge.
            let saturated: Vec<usize> = (0..n_edges)
                .filter(|&e| remaining[e] <= EPS)
                .collect();
            if saturated.is_empty() {
                // No edge tightened — defensively bail to avoid spinning.
                break;
            }
            for i in 0..n_flows {
                if !active[i] {
                    continue;
                }
                if flow_edges[i].iter().any(|e| saturated.contains(e)) {
                    active[i] = false;
                }
            }
            if !active.iter().any(|&a| a) {
                break;
            }
        }

        // Build the snapshot.
        for (i, c) in commodities.iter().enumerate() {
            let total = rates[i];
            if total <= 0.0 {
                continue;
            }
            let fid = c.flow_id();
            snap.rates.insert(fid.clone(), total);
            if let Some(p) = c.paths.first() {
                for w in p.windows(2) {
                    let u = &w[0];
                    let nxt = &w[1];
                    let qkc_ft = snap.forwarding.entry(u.clone()).or_default();
                    let entries = qkc_ft.entry(fid.clone()).or_default();
                    if let Some(slot) = entries.iter_mut().find(|(h, _)| h == nxt) {
                        slot.1 += 1.0;
                    } else {
                        entries.push((nxt.clone(), 1.0));
                    }
                }
            }
            snap.rates_by_dkms
                .entry(c.src_dkms.clone())
                .or_default()
                .insert((c.dst_dkms.clone(), BufferRole::EncKeys), total);
            snap.rates_by_dkms
                .entry(c.dst_dkms.clone())
                .or_default()
                .insert((c.src_dkms.clone(), BufferRole::DecKeys), total);
        }

        debug!(
            flows = n_flows,
            edges = n_edges,
            iters = guard,
            "weighted_maxmin done"
        );
        snap
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::{Dkms, EdgeMeta, HostEndpoint, Orr, Qkc};

    fn graph_from(edges: &[(&str, &str)]) -> HashMap<String, HashSet<String>> {
        let mut g: HashMap<String, HashSet<String>> = HashMap::new();
        for (a, b) in edges {
            g.entry((*a).into()).or_default().insert((*b).into());
            g.entry((*b).into()).or_default().insert((*a).into());
        }
        g
    }

    #[test]
    fn k_shortest_picks_disjoint_alternatives() {
        // Graph:
        //   1 -- 2 -- 4
        //   1 -- 3 -- 4
        // Two equal-length paths of length 3.
        let g = graph_from(&[("1", "2"), ("2", "4"), ("1", "3"), ("3", "4")]);
        let paths = k_shortest_paths(&g, "1", "4", 2);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| p.len() == 3));
        assert_ne!(paths[0], paths[1]);
    }

    #[test]
    fn k_shortest_empty_on_disconnected() {
        let g = graph_from(&[("1", "2"), ("3", "4")]);
        assert!(k_shortest_paths(&g, "1", "4", 3).is_empty());
    }

    fn host(id: i64) -> HostEndpoint {
        HostEndpoint {
            id,
            ip: format!("10.0.0.{id}"),
            port: 9000 + id as u16,
        }
    }

    fn small_topo() -> Topology {
        // QKCs: 1 - 2 - 3 (linear).
        // DKMSs: dA@1, dB@3.
        let mut t = Topology::default();
        for q in ["1", "2", "3"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 100.0,
                alpha: 0.2,
                max_buffer_size: 10,
            },
        );
        t.add_edge(
            "2",
            "3",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 100.0,
                alpha: 0.2,
                max_buffer_size: 10,
            },
        );
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: host(11),
            qkc_id: "1".into(),
        });
        t.upsert_orr(Orr {
            id: "o3".into(),
            host: host(13),
            qkc_id: "3".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dA".into(),
            host: host(21),
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dB".into(),
            host: host(23),
            tls_id: None,
            orr_id: "o3".into(),
        });
        t
    }

    #[test]
    fn solver_assigns_rates_to_all_commodities() {
        let topo = small_topo();
        let s = McfSolver::new(2);
        let coms = s.build_commodities(&topo);
        // dA→dB and dB→dA (different commodities, independent flows).
        assert_eq!(coms.len(), 2);
        let caps = s.capacities(&topo);
        let snap = s.solve(&coms, &caps, &HashMap::new());
        for c in &coms {
            assert!(snap.rates.get(&c.flow_id()).copied().unwrap_or(0.0) > 0.0);
        }
        // Buffer view: dA pushes ENC to dB, dB receives DEC from dA.
        let r_ab = snap.rate_for_flow("dA", "dB");
        assert!(
            (snap.rates_by_dkms["dA"][&("dB".to_string(), BufferRole::EncKeys)] - r_ab).abs()
                < 1e-9
        );
        assert!(
            (snap.rates_by_dkms["dB"][&("dA".to_string(), BufferRole::DecKeys)] - r_ab).abs()
                < 1e-9
        );
    }

    #[test]
    fn solver_respects_capacity_bound() {
        let topo = small_topo();
        let s = McfSolver::new(2);
        let coms = s.build_commodities(&topo);
        let caps = s.capacities(&topo);
        let snap = s.solve(&coms, &caps, &HashMap::new());
        // No edge can carry more than its cap.
        for (k, cap) in &caps {
            let mut usage = 0.0;
            for c in &coms {
                let r = snap.rates.get(&c.flow_id()).copied().unwrap_or(0.0);
                if r <= 0.0 {
                    continue;
                }
                if let Some(p) = c.paths.first() {
                    if p.windows(2).any(|w| &edge_key(&w[0], &w[1]) == k) {
                        usage += r;
                    }
                }
            }
            assert!(
                usage <= cap * 1.01 + 1e-6,
                "edge {:?} usage {} > cap {}",
                k,
                usage,
                cap
            );
        }
    }

    #[test]
    fn hybrid_solver_starves_low_tier_when_high_tier_uses_full_capacity() {
        // Two pairs share a 1 kps edge. dA↔dB weight 10000 (HIGH tier,
        // Priority), dC↔dD weight 1 (LOW tier, BestEffort). The hybrid
        // solver runs HIGH first on the full capacity; LOW only sees
        // whatever HIGH leaves over — which here is zero.
        let mut t = Topology::default();
        for q in ["1", "2"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 1.0,
                alpha: 0.0,
                max_buffer_size: 1,
            },
        );
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: host(11),
            qkc_id: "1".into(),
        });
        t.upsert_orr(Orr {
            id: "o2".into(),
            host: host(12),
            qkc_id: "2".into(),
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
            orr_id: "o2".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dC".into(),
            host: host(23),
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "dD".into(),
            host: host(24),
            tls_id: None,
            orr_id: "o2".into(),
        });

        let s = McfSolver::new(1);
        let coms = s.build_commodities(&t);
        let caps = s.capacities(&t);
        let mut weights = HashMap::new();
        for c in &coms {
            let w = if c.src_dkms == "dA" || c.src_dkms == "dB" {
                10_000.0 // Priority (HIGH tier)
            } else {
                1.0 // BestEffort (LOW tier)
            };
            weights.insert(c.flow_id(), w);
        }
        let snap = s.solve(&coms, &caps, &weights);

        let r_high = snap.rate_for_flow("dA", "dB");
        let r_low = snap.rate_for_flow("dC", "dD");
        assert!(r_high > 0.0, "HIGH tier got 0 — it should consume the edge");
        assert!(
            r_low <= 1e-6,
            "LOW tier should be starved when HIGH saturates the edge; got {r_low}",
        );
    }

    #[test]
    fn hybrid_solver_splits_intra_tier_by_weight_ratio() {
        // Same single edge, but both pairs are in the HIGH tier.
        // dA↔dB at Priority (10000), dC↔dD at Quickly (100). Same tier,
        // so weighted max-min applies: dA gets ≈100× the rate of dC,
        // and neither is starved.
        let mut t = Topology::default();
        for q in ["1", "2"] {
            t.upsert_qkc(Qkc {
                id: q.into(),
                host: host(q.parse().unwrap()),
                kme_host: None,
            });
        }
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                distance_km: 0,
                r0_keys_per_second: 1.0,
                alpha: 0.0,
                max_buffer_size: 1,
            },
        );
        t.upsert_orr(Orr { id: "o1".into(), host: host(11), qkc_id: "1".into() });
        t.upsert_orr(Orr { id: "o2".into(), host: host(12), qkc_id: "2".into() });
        for (i, q) in [("dA", "1"), ("dB", "2"), ("dC", "1"), ("dD", "2")]
            .iter()
            .enumerate()
        {
            let orr = if q.1 == "1" { "o1" } else { "o2" };
            t.upsert_dkms(Dkms {
                id: q.0.into(),
                host: host(21 + i as i64),
                tls_id: None,
                orr_id: orr.into(),
            });
        }
        let s = McfSolver::new(1);
        let coms = s.build_commodities(&t);
        let caps = s.capacities(&t);
        let mut weights = HashMap::new();
        for c in &coms {
            let w = if c.src_dkms == "dA" || c.src_dkms == "dB" {
                10_000.0 // Priority
            } else {
                100.0 // Quickly — same HIGH tier
            };
            weights.insert(c.flow_id(), w);
        }
        let snap = s.solve(&coms, &caps, &weights);

        let r_pri = snap.rate_for_flow("dA", "dB");
        let r_qly = snap.rate_for_flow("dC", "dD");
        assert!(r_pri > 0.0 && r_qly > 0.0, "neither should be starved within HIGH tier");
        let ratio = r_pri / r_qly;
        assert!(
            (90.0..=110.0).contains(&ratio),
            "expected ratio ≈100 within tier, got {ratio}",
        );
    }
}
