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

use tracing::{debug, info};

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
    pub fn solve(
        &self,
        commodities: &[Commodity],
        capacities: &HashMap<EdgeKey, f64>,
        weights: &HashMap<String, f64>,
    ) -> McfSnapshot {
        self.strict_priority_within_class(commodities, capacities, weights)
    }

    // -------------------------------------------------------- SP + RR

    fn strict_priority_within_class(
        &self,
        commodities: &[Commodity],
        capacities: &HashMap<EdgeKey, f64>,
        weights: &HashMap<String, f64>,
    ) -> McfSnapshot {
        if commodities.is_empty() {
            return McfSnapshot::default();
        }

        // Bucket commodities by their declared weight, descending.
        let mut by_weight: BTreeMap<OrderedF64, Vec<&Commodity>> = BTreeMap::new();
        for c in commodities {
            let w = weights.get(&c.flow_id()).copied().unwrap_or(1.0);
            by_weight.entry(OrderedF64(w)).or_default().push(c);
        }

        let mut remaining_caps: HashMap<EdgeKey, f64> = capacities.clone();
        let mut snap = McfSnapshot::default();

        info!(
            classes = by_weight.len(),
            total_flows = commodities.len(),
            edges = remaining_caps.len(),
            "strict_priority start"
        );

        // Iterate descending — BTreeMap is ascending, so reverse.
        for (OrderedF64(cls_w), class_flows) in by_weight.iter().rev() {
            if class_flows.is_empty() {
                continue;
            }
            if remaining_caps.values().all(|c| *c <= 1e-9) {
                info!(
                    weight = cls_w,
                    flows = class_flows.len(),
                    "class skipped (capacity exhausted)"
                );
                continue;
            }

            // Within a class everybody has equal share (the priority is
            // expressed by the class, not by weight magnitude).
            let equal_weights: HashMap<String, f64> =
                class_flows.iter().map(|c| (c.flow_id(), 1.0)).collect();

            let sub = self.proportional_fair(class_flows, &remaining_caps, &equal_weights);

            // Merge sub-snapshot into the global one.
            for (fid, r) in &sub.rates {
                snap.rates.insert(fid.clone(), *r);
            }
            for (dkms, m) in &sub.rates_by_dkms {
                snap.rates_by_dkms
                    .entry(dkms.clone())
                    .or_default()
                    .extend(m.clone());
            }
            for (u, ft) in &sub.forwarding {
                let merged = snap.forwarding.entry(u.clone()).or_default();
                for (fid, entries) in ft {
                    merged.insert(fid.clone(), entries.clone());
                }
            }

            // Subtract usage along each flow's shortest path.
            for c in class_flows {
                let r = sub.rates.get(&c.flow_id()).copied().unwrap_or(0.0);
                if r <= 0.0 {
                    continue;
                }
                let Some(p) = c.paths.first() else { continue };
                for (u, v) in p.iter().zip(p.iter().skip(1)) {
                    let k = edge_key(u, v);
                    if let Some(cap) = remaining_caps.get_mut(&k) {
                        *cap = (*cap - r).max(0.0);
                    }
                }
            }

            debug!(
                weight = cls_w,
                flows = class_flows.len(),
                cap_min = remaining_caps
                    .values()
                    .copied()
                    .fold(f64::INFINITY, f64::min),
                "class done"
            );
        }

        snap
    }

    // -------------------------------------------------------- Water-filling
    //
    // Proportional-fair single-path solver. Pure-Rust port of the
    // Python `_solve_proportional_fair`.

    fn proportional_fair(
        &self,
        commodities: &[&Commodity],
        capacities: &HashMap<EdgeKey, f64>,
        weights: &HashMap<String, f64>,
    ) -> McfSnapshot {
        let mut snap = McfSnapshot::default();
        if commodities.is_empty() || capacities.is_empty() {
            return snap;
        }

        // Stable edge ordering — required so the solver is reproducible
        // even when the capacity HashMap iterates in arbitrary order.
        let edge_list: Vec<EdgeKey> = {
            let mut v: Vec<_> = capacities.keys().cloned().collect();
            v.sort();
            v
        };
        let edge_idx: HashMap<&EdgeKey, usize> =
            edge_list.iter().enumerate().map(|(i, k)| (k, i)).collect();
        let n_edges = edge_list.len();
        let n_flows = commodities.len();

        // Per-flow: list of edge indices traversed by its first path.
        // Per-edge: list of flow indices passing through it.
        let mut flow_edges: Vec<Vec<usize>> = vec![vec![]; n_flows];
        let mut edge_flows: Vec<Vec<usize>> = vec![vec![]; n_edges];
        for (i, c) in commodities.iter().enumerate() {
            let Some(p) = c.paths.first() else { continue };
            for ek in path_edges(p) {
                if let Some(&e) = edge_idx.get(&ek) {
                    flow_edges[i].push(e);
                    edge_flows[e].push(i);
                }
            }
        }

        let weights_arr: Vec<f64> = commodities
            .iter()
            .map(|c| weights.get(&c.flow_id()).copied().unwrap_or(1.0).max(1e-9))
            .collect();
        let caps: Vec<f64> = edge_list.iter().map(|k| capacities[k]).collect();

        // Heuristic init: λ_e starts at the count of flows that touch
        // edge e (at least 1.0). Same shape as the Python init —
        // `np.maximum(total_demand_per_edge, 1.0)`.
        let mut lam: Vec<f64> = edge_flows
            .iter()
            .map(|fs| (fs.len() as f64).max(1.0))
            .collect();

        // Hyper-parameters — same defaults as the Python (no env-var
        // override; we'll add a config knob if it turns out to matter).
        let max_iter = 200usize;
        let tol = 1e-3_f64;
        let eta = 0.5_f64;
        const EPS: f64 = 1e-9;
        const LAM_LO: f64 = 1e-12;
        const LAM_HI: f64 = 1e12;

        let mut rates = vec![0.0_f64; n_flows];
        let mut viol = f64::INFINITY;
        let mut iter_count = 0;

        for it in 0..max_iter {
            iter_count = it + 1;

            // denom[i] = Σ λ_e for e in flow i's path.
            for i in 0..n_flows {
                let mut s = 0.0;
                for &e in &flow_edges[i] {
                    s += lam[e];
                }
                rates[i] = weights_arr[i] / s.max(EPS);
            }

            // usage[e] = Σ rates[i] for i flowing through e.
            // rel_excess = (usage − cap) / cap.
            // λ ← λ · exp(η · rel_excess), clipped.
            viol = 0.0;
            for e in 0..n_edges {
                let mut usage = 0.0;
                for &i in &edge_flows[e] {
                    usage += rates[i];
                }
                let cap = caps[e].max(EPS);
                let rel = (usage - caps[e]) / cap;
                viol = viol.max(rel.abs());
                lam[e] = (lam[e] * (eta * rel).exp()).clamp(LAM_LO, LAM_HI);
            }

            if viol < tol {
                break;
            }
        }

        // Defensive projection: if an edge is still over cap by more
        // than `tol`, scale down the flows that share it.
        for e in 0..n_edges {
            let usage: f64 = edge_flows[e].iter().map(|&i| rates[i]).sum();
            if usage > caps[e] * (1.0 + tol) && usage > EPS {
                let scale = caps[e] / usage;
                for &i in &edge_flows[e] {
                    rates[i] *= scale;
                }
            }
        }
        for r in &mut rates {
            if *r < 0.0 {
                *r = 0.0;
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

            // Single-path: omega = 1.0 along the first path.
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
            iters = iter_count,
            viol = viol,
            "proportional_fair done"
        );
        snap
    }
}

// f64 wrapper with total ordering, for use as BTreeMap key in
// strict_priority_within_class. NaN is sorted as the smallest value.
#[derive(Debug, Clone, Copy)]
struct OrderedF64(f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, o: &Self) -> bool {
        self.0.to_bits() == o.0.to_bits()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.0
            .partial_cmp(&o.0)
            .unwrap_or(std::cmp::Ordering::Equal)
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
    fn strict_priority_starves_lower_class_when_higher_saturates() {
        // 1 - 2, single shared edge with very small capacity.
        // Two DKMSs at each end: dA, dB. Build a separate priority pair
        // by adding a 4th DKMS dC@1 → dD@3 with weight 1.0, while dA→dB
        // gets weight 100.0.
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
        // dA→dB and dB→dA are class 100; dC→dD and dD→dC are class 1.
        for c in &coms {
            let w = if c.src_dkms == "dA" || c.src_dkms == "dB" {
                100.0
            } else {
                1.0
            };
            weights.insert(c.flow_id(), w);
        }
        let snap = s.solve(&coms, &caps, &weights);

        // Higher-priority flows get rate; lower-priority flows are starved.
        assert!(snap.rate_for_flow("dA", "dB") > 0.0);
        assert!(snap.rate_for_flow("dC", "dD") <= 1e-6);
    }
}
