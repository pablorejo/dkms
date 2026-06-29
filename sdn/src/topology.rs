//! In-memory topology graph for the SDN.
//!
//! Mirrors the entity model of the Python SDN (`code_dkms/src/SDN/topology.py`):
//! the SDN owns five kinds of objects — QKC, ORR, DKMS, SAE and the physical
//! QKC↔QKC links — plus the undirected adjacency graph derived from those
//! links.
//!
//! Snapshots are kept in an [`ArcSwap`] so readers (gRPC, HTTP, MCF solver)
//! never block; writers serialize through a [`parking_lot::Mutex`], clone the
//! current snapshot, mutate, and atomically swap it in.

use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::error::{Result, SdnError};

// ---------------------------------------------------------------- entities

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostEndpoint {
    pub id: i64,
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Qkc {
    pub id: String,
    pub host: HostEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kme_host: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Orr {
    pub id: String,
    pub host: HostEndpoint,
    /// Id of the QKC this ORR belongs to.
    pub qkc_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dkms {
    pub id: String,
    pub host: HostEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_id: Option<i64>,
    /// Id of the ORR this DKMS uses (which in turn is anchored to one QKC).
    pub orr_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Sae {
    pub id: String,
    /// Id of the DKMS that serves this SAE.
    pub dkms_id: String,
}

/// Input item for `register_sae_bulk` — same shape as the per-item args
/// of `register_sae`. Either `dkms_id` (canonical) or `dkms_target`
/// (legacy host:port) must be set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaeBulkItem {
    pub sae_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dkms_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dkms_target: Option<(String, u16)>,
}

/// Outcome of one SAE in a `register_sae_bulk` call. Failures do NOT
/// abort the batch; each item gets its own row.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum BulkSaeOutcome {
    Ok(Sae),
    Error {
        sae_id: String,
        code: String,
        detail: String,
    },
}

/// Channel kind of a QKC↔QKC link.
///
/// * `Qkd` (default) — keys come from the shared quditto; capacity is the
///   distance-attenuated QKD rate (see [`EdgeMeta::quditto_capacity_keys_per_second`]).
/// * `Pqc` — keys are derived from an ML-KEM secret in the QKC; the link is
///   not QKD-rate-limited, so the MCMCF-λ solver treats it as **uncapacitated**
///   (routable, no capacity constraint).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkType {
    #[default]
    Qkd,
    Pqc,
}

/// Physical metadata of a QKC↔QKC link.
///
/// `r0_keys_per_second` × 10^(−α·d/10) gives the link's effective key-rate
/// capacity following the Quditto model used in the Python SDN.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EdgeMeta {
    #[serde(default)]
    pub distance_km: u32,
    #[serde(default = "default_r0")]
    pub r0_keys_per_second: f64,
    #[serde(default = "default_alpha")]
    pub alpha: f64,
    #[serde(default = "default_buf_size")]
    pub max_buffer_size: u32,
    /// QKD (default) or PQC. PQC edges are uncapacitated in the MCF.
    #[serde(default)]
    pub link_type: LinkType,
}

fn default_r0() -> f64 {
    20.0
}
fn default_alpha() -> f64 {
    0.2
}
fn default_buf_size() -> u32 {
    100
}

impl Default for EdgeMeta {
    fn default() -> Self {
        Self {
            distance_km: 0,
            r0_keys_per_second: default_r0(),
            alpha: default_alpha(),
            max_buffer_size: default_buf_size(),
            link_type: LinkType::default(),
        }
    }
}

impl EdgeMeta {
    /// `C_e = R₀ · 10^(−α·d/10)` (keys/s).
    pub fn quditto_capacity_keys_per_second(&self) -> f64 {
        let exp = -self.alpha * self.distance_km as f64 / 10.0;
        (self.r0_keys_per_second * 10f64.powf(exp)).max(0.0)
    }

    /// `true` for PQC links (uncapacitated in the MCF).
    pub fn is_pqc(&self) -> bool {
        self.link_type == LinkType::Pqc
    }
}

/// Sorted pair of QKC ids — canonical key for an undirected edge.
pub type EdgeKey = (String, String);

pub fn edge_key(a: &str, b: &str) -> EdgeKey {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

// ----------------------------------------------------------------- snapshot

/// Full in-memory view of the SDN topology.
///
/// Cloning this is `O(n)` on the entity counts; we accept that because
/// mutations are rare relative to reads (every gRPC stream, HTTP endpoint and
/// MCF solve hits a snapshot).
#[derive(Debug, Default, Clone)]
pub struct Topology {
    pub qkcs: HashMap<String, Qkc>,
    pub orrs: HashMap<String, Orr>,
    /// Convenience index: QKC id → ORR id that lives on it.
    pub orr_by_qkc: HashMap<String, String>,
    pub dkms: HashMap<String, Dkms>,
    /// Convenience index: QKC id → first DKMS anchored to it.
    pub dkms_by_qkc: HashMap<String, String>,
    pub saes: HashMap<String, Sae>,

    /// Undirected adjacency between QKCs.
    pub graph: HashMap<String, HashSet<String>>,
    /// Physical metadata, keyed by sorted (qkc_a, qkc_b).
    pub edges: HashMap<EdgeKey, EdgeMeta>,

    /// Monotonically increasing snapshot version. Bumped on every mutation.
    pub version: i64,
    /// Folder we last loaded from, if any.
    pub loaded_from: Option<PathBuf>,
}

impl Topology {
    // ---------- DKMS helpers

    /// QKC id where a given DKMS lives (via its ORR), if both exist.
    pub fn qkc_of_dkms(&self, dkms_id: &str) -> Option<&str> {
        let d = self.dkms.get(dkms_id)?;
        let o = self.orrs.get(&d.orr_id)?;
        Some(o.qkc_id.as_str())
    }

    /// Resolve a DKMS by id, or by (ip, port) if id is None.
    pub fn resolve_dkms(&self, id: Option<&str>, ip_port: Option<(&str, u16)>) -> Option<&Dkms> {
        if let Some(id) = id {
            if let Some(d) = self.dkms.get(id) {
                return Some(d);
            }
        }
        if let Some((ip, port)) = ip_port {
            return self
                .dkms
                .values()
                .find(|d| d.host.ip == ip && d.host.port == port);
        }
        None
    }

    // ---------- routing

    /// Shortest path (in hops) between two QKCs. Returns the full node list
    /// including endpoints, or `None` if unreachable.
    pub fn shortest_path_qkc(&self, src: &str, dst: &str) -> Option<Vec<String>> {
        if !self.graph.contains_key(src) || !self.graph.contains_key(dst) {
            return None;
        }
        if src == dst {
            return Some(vec![src.to_string()]);
        }
        let mut prev: HashMap<String, Option<String>> = HashMap::new();
        prev.insert(src.to_string(), None);
        let mut queue: VecDeque<String> = VecDeque::from([src.to_string()]);
        while let Some(node) = queue.pop_front() {
            // Sorted neighbors → deterministic BFS, matching Python.
            let mut neighbors: Vec<&String> = self
                .graph
                .get(&node)
                .map(|s| s.iter().collect())
                .unwrap_or_default();
            neighbors.sort();
            for n in neighbors {
                if prev.contains_key(n) {
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

    /// Next hop QKC toward `dst` from `src`. Mirrors the Python helper.
    pub fn next_hop_qkc(&self, src: &str, dst: &str) -> Option<String> {
        let path = self.shortest_path_qkc(src, dst)?;
        if src == dst {
            return Some(src.to_string());
        }
        path.get(1).cloned()
    }

    pub fn edge(&self, a: &str, b: &str) -> Option<&EdgeMeta> {
        self.edges.get(&edge_key(a, b))
    }

    // ---------- QKD-subgraph connectivity (security grades)

    /// Connected components of the **QKD-only subgraph** (edges with
    /// `!is_pqc()`), as QKC id → component index. A QKC with no QKD edge is
    /// its own singleton component. PQC edges are ignored: two QKCs share a
    /// component iff a strictly-QKD path links them. Component ids are stable
    /// (assigned in sorted QKC order) so the map is deterministic.
    ///
    /// This is the primitive behind the per-request security grade: a pair is
    /// QKD-reachable (so `strict_qkd`/`qkd_prefer` can be served QKD-grade)
    /// iff its two QKCs land in the same component.
    pub fn qkd_components(&self) -> HashMap<String, usize> {
        let mut comp: HashMap<String, usize> = HashMap::new();
        let mut nodes: Vec<&String> = self.qkcs.keys().collect();
        nodes.sort();
        let mut next = 0usize;
        for start in nodes {
            if comp.contains_key(start) {
                continue;
            }
            let id = next;
            next += 1;
            comp.insert(start.clone(), id);
            let mut queue: VecDeque<String> = VecDeque::from([start.clone()]);
            while let Some(node) = queue.pop_front() {
                let Some(neigh) = self.graph.get(&node) else {
                    continue;
                };
                let mut ns: Vec<&String> = neigh.iter().collect();
                ns.sort();
                for n in ns {
                    if comp.contains_key(n) {
                        continue;
                    }
                    // Traverse only strictly-QKD links. Missing edge meta
                    // (shouldn't happen for a graph edge) is treated as non-QKD.
                    if self.edge(&node, n).is_none_or(EdgeMeta::is_pqc) {
                        continue;
                    }
                    comp.insert(n.clone(), id);
                    queue.push_back(n.clone());
                }
            }
        }
        comp
    }

    /// `true` iff a strictly-QKD path connects the two QKCs (same QKD
    /// component). `a == b` is trivially true. Recomputes components on each
    /// call — for many pairs, call [`Self::qkd_components`] once and compare.
    pub fn qkd_connected_qkc(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let comp = self.qkd_components();
        matches!((comp.get(a), comp.get(b)), (Some(x), Some(y)) if x == y)
    }
}

// ---------------------------------------------------------------- mutations
// All mutating methods return whether anything actually changed, so writers
// can decide whether to bump the version / re-publish.

impl Topology {
    pub fn upsert_qkc(&mut self, qkc: Qkc) -> bool {
        let key = qkc.id.clone();
        let changed = self.qkcs.get(&key) != Some(&qkc);
        self.graph.entry(key.clone()).or_default();
        self.qkcs.insert(key, qkc);
        changed
    }

    pub fn upsert_orr(&mut self, orr: Orr) -> bool {
        if !self.qkcs.contains_key(&orr.qkc_id) {
            warn!(orr=%orr.id, qkc=%orr.qkc_id, "ORR references unknown QKC; skipping");
            return false;
        }
        let key = orr.id.clone();
        let changed = self.orrs.get(&key) != Some(&orr);
        self.orr_by_qkc.insert(orr.qkc_id.clone(), key.clone());
        self.orrs.insert(key, orr);
        changed
    }

    pub fn upsert_dkms(&mut self, dkms: Dkms) -> bool {
        let Some(orr) = self.orrs.get(&dkms.orr_id) else {
            warn!(dkms=%dkms.id, orr=%dkms.orr_id, "DKMS references unknown ORR; skipping");
            return false;
        };
        let qkc_id = orr.qkc_id.clone();
        let key = dkms.id.clone();
        let changed = self.dkms.get(&key) != Some(&dkms);
        // First-write-wins on the dkms_by_qkc index — Python keeps the latest
        // overwrite, we mirror that to stay 1:1.
        self.dkms_by_qkc.insert(qkc_id, key.clone());
        self.dkms.insert(key, dkms);
        changed
    }

    pub fn upsert_sae(&mut self, sae: Sae) -> bool {
        if !self.dkms.contains_key(&sae.dkms_id) {
            warn!(sae=%sae.id, dkms=%sae.dkms_id, "SAE references unknown DKMS; skipping");
            return false;
        }
        let key = sae.id.clone();
        let changed = self.saes.get(&key) != Some(&sae);
        self.saes.insert(key, sae);
        changed
    }

    pub fn remove_sae(&mut self, sae_id: &str) -> bool {
        self.saes.remove(sae_id).is_some()
    }

    pub fn add_edge(&mut self, a: &str, b: &str, meta: EdgeMeta) -> bool {
        if a == b {
            return false;
        }
        // Both endpoints must exist as QKCs.
        if !self.qkcs.contains_key(a) || !self.qkcs.contains_key(b) {
            warn!(a, b, "edge endpoints not registered as QKCs; skipping");
            return false;
        }
        self.graph
            .entry(a.to_string())
            .or_default()
            .insert(b.to_string());
        self.graph
            .entry(b.to_string())
            .or_default()
            .insert(a.to_string());
        let key = edge_key(a, b);
        match self.edges.get_mut(&key) {
            Some(existing) => {
                // Merge in better information (Python keeps the first
                // non-zero distance and the latest channel params).
                let mut changed = false;
                if existing.distance_km == 0 && meta.distance_km > 0 {
                    existing.distance_km = meta.distance_km;
                    changed = true;
                }
                if (existing.r0_keys_per_second - meta.r0_keys_per_second).abs() > f64::EPSILON {
                    existing.r0_keys_per_second = meta.r0_keys_per_second;
                    changed = true;
                }
                if (existing.alpha - meta.alpha).abs() > f64::EPSILON {
                    existing.alpha = meta.alpha;
                    changed = true;
                }
                if existing.max_buffer_size != meta.max_buffer_size {
                    existing.max_buffer_size = meta.max_buffer_size;
                    changed = true;
                }
                changed
            }
            None => {
                self.edges.insert(key, meta);
                true
            }
        }
    }

    /// Set link capacity expressed in keys/s. Inverts the Quditto formula to
    /// recompute `R₀` for the link. Returns `Some(true)` if it changed.
    pub fn set_edge_capacity_kps(&mut self, a: &str, b: &str, capacity_kps: f64) -> Option<bool> {
        if capacity_kps < 0.0 {
            return None;
        }
        let key = edge_key(a, b);
        let edge = self.edges.get_mut(&key)?;
        // PQC edges are uncapacitated: capacity edits are a no-op.
        if edge.is_pqc() {
            return Some(false);
        }
        let current = edge.quditto_capacity_keys_per_second();
        let threshold = (current * 0.05).max(0.5);
        if (current - capacity_kps).abs() < threshold {
            return Some(false);
        }
        let exp = edge.alpha * edge.distance_km as f64 / 10.0;
        edge.r0_keys_per_second = capacity_kps * 10f64.powf(exp);
        Some(true)
    }
}

// ----------------------------------------------------------------- loader

/// Iterate config JSONs for one entity kind. Looks at both `<base>/<Prefix>/`
/// (preferred) and `<base>/<Prefix>_*.json` (legacy flat layout).
fn iter_entity_files(base: &Path, prefix: &str) -> Vec<PathBuf> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    let nested = base.join(prefix);
    if nested.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&nested) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json") {
                    out.insert(p);
                }
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(base) {
        let needle = format!("{prefix}_");
        for entry in rd.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with(&needle) && name.ends_with(".json") {
                out.insert(p);
            }
        }
    }
    out.into_iter().collect()
}

fn read_json(path: &Path) -> Result<Value> {
    let raw = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&raw)?)
}

fn json_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn parse_host(value: &Value) -> Option<HostEndpoint> {
    let host_obj = value.get("host").or(Some(value))?;
    let id_v = host_obj
        .get("id")
        .or_else(|| value.get("host_id"))
        .or_else(|| value.get("id_host"))?;
    let ip_v = host_obj.get("ip").or_else(|| value.get("ip"))?;
    let port_v = host_obj.get("port").or_else(|| value.get("port"))?;
    Some(HostEndpoint {
        id: id_v.as_i64()?,
        ip: ip_v.as_str()?.to_string(),
        port: u16::try_from(port_v.as_i64()?).ok()?,
    })
}

fn parse_edge_meta(channel: &Value) -> EdgeMeta {
    let m = |k: &str| channel.get(k);
    EdgeMeta {
        distance_km: m("distance").and_then(Value::as_i64).unwrap_or(0).max(0) as u32,
        r0_keys_per_second: m("quditto_rate_r0")
            .or_else(|| m("rate_r0"))
            .and_then(Value::as_f64)
            .unwrap_or(default_r0()),
        alpha: m("quditto_rate_alpha")
            .or_else(|| m("rate_alpha"))
            .and_then(Value::as_f64)
            .unwrap_or(default_alpha()),
        max_buffer_size: m("quditto_max_buffer_size")
            .or_else(|| m("max_buffer_size"))
            .and_then(Value::as_i64)
            .unwrap_or(default_buf_size() as i64)
            .max(0) as u32,
        link_type: match m("link_type").and_then(Value::as_str) {
            Some(s) if s.eq_ignore_ascii_case("pqc") => LinkType::Pqc,
            _ => LinkType::Qkd, // ausente o cualquier otro valor → QKD (compat)
        },
    }
}

impl Topology {
    /// Build a topology from a Python-style config folder.
    ///
    /// Expected layout (any subset is acceptable):
    ///
    /// ```text
    /// <folder>/QKC/*.json
    /// <folder>/ORR/*.json
    /// <folder>/DKMS/*.json
    /// <folder>/SAE/*.json
    /// ```
    ///
    /// or the legacy flat form `<folder>/QKC_<id>.json`, etc. Load order is
    /// QKC → ORR → DKMS → SAE so foreign-key references resolve in one pass.
    pub fn load_from_folder(folder: &Path) -> Result<Self> {
        if !folder.is_dir() {
            return Err(SdnError::Topology(format!(
                "config folder does not exist: {}",
                folder.display()
            )));
        }
        let mut t = Topology {
            loaded_from: Some(folder.to_path_buf()),
            ..Topology::default()
        };

        for path in iter_entity_files(folder, "QKC") {
            if let Err(e) = t.load_qkc_file(&path) {
                warn!(file = %path.display(), error = %e, "failed to load QKC file");
            }
        }
        for path in iter_entity_files(folder, "ORR") {
            if let Err(e) = t.load_orr_file(&path) {
                warn!(file = %path.display(), error = %e, "failed to load ORR file");
            }
        }
        for path in iter_entity_files(folder, "DKMS") {
            if let Err(e) = t.load_dkms_file(&path) {
                warn!(file = %path.display(), error = %e, "failed to load DKMS file");
            }
        }
        for path in iter_entity_files(folder, "SAE") {
            if let Err(e) = t.load_sae_file(&path) {
                warn!(file = %path.display(), error = %e, "failed to load SAE file");
            }
        }

        t.version = 1;
        info!(
            folder = %folder.display(),
            qkcs = t.qkcs.len(),
            orrs = t.orrs.len(),
            dkms = t.dkms.len(),
            saes = t.saes.len(),
            edges = t.edges.len(),
            "topology loaded"
        );
        Ok(t)
    }

    fn load_qkc_file(&mut self, path: &Path) -> Result<()> {
        let v = read_json(path)?;
        let id = v
            .get("id")
            .and_then(json_str)
            .or_else(|| v.get("QKC_id").and_then(json_str))
            .ok_or_else(|| SdnError::Topology(format!("QKC file {} has no id", path.display())))?;
        let host = parse_host(&v)
            .ok_or_else(|| SdnError::Topology(format!("QKC {id} missing host info")))?;
        let qkc = Qkc {
            id: id.clone(),
            host,
            kme_host: v.get("kme_host").and_then(Value::as_str).map(String::from),
        };
        self.upsert_qkc(qkc);

        // Inline KME/neighbor channel info, if present, produces edges.
        if let Some(kmes) = v.get("kmes").and_then(Value::as_array) {
            for kme in kmes {
                let neighbor = kme
                    .get("neighbor_qkc_id")
                    .or_else(|| kme.get("id_nei"))
                    .and_then(json_str);
                let Some(neighbor) = neighbor else { continue };
                let channel = kme.get("channel").cloned().unwrap_or_else(|| kme.clone());
                let meta = parse_edge_meta(&channel);
                // Endpoint may not be loaded yet — register it as a stub QKC
                // so the graph stays consistent. The real QKC entry will
                // overwrite the stub when its own file is processed.
                if !self.qkcs.contains_key(&neighbor) {
                    debug!(neighbor=%neighbor, "registering placeholder QKC for edge");
                    self.graph.entry(neighbor.clone()).or_default();
                }
                self.add_edge(&id, &neighbor, meta);
            }
        }
        Ok(())
    }

    fn load_orr_file(&mut self, path: &Path) -> Result<()> {
        let v = read_json(path)?;
        let id = v
            .get("id")
            .and_then(json_str)
            .ok_or_else(|| SdnError::Topology(format!("ORR file {} has no id", path.display())))?;
        let qkc_id = v
            .get("qkc_id")
            .or_else(|| v.get("QKC_id"))
            .or_else(|| v.get("id_qkc"))
            .and_then(json_str)
            .ok_or_else(|| SdnError::Topology(format!("ORR {id} missing qkc_id")))?;
        let host = parse_host(&v)
            .ok_or_else(|| SdnError::Topology(format!("ORR {id} missing host info")))?;
        self.upsert_orr(Orr { id, host, qkc_id });
        Ok(())
    }

    fn load_dkms_file(&mut self, path: &Path) -> Result<()> {
        let v = read_json(path)?;
        let id = v
            .get("id")
            .and_then(json_str)
            .or_else(|| v.get("id_dkms").and_then(json_str))
            .ok_or_else(|| SdnError::Topology(format!("DKMS file {} has no id", path.display())))?;
        let orr_id = v
            .get("orr_id")
            .or_else(|| v.get("ORR_id"))
            .or_else(|| v.get("id_orr"))
            .and_then(json_str)
            .ok_or_else(|| SdnError::Topology(format!("DKMS {id} missing orr_id")))?;
        let host = parse_host(&v)
            .ok_or_else(|| SdnError::Topology(format!("DKMS {id} missing host info")))?;
        let tls_id = v.get("tls_id").and_then(Value::as_i64);
        self.upsert_dkms(Dkms {
            id,
            host,
            tls_id,
            orr_id,
        });
        Ok(())
    }

    fn load_sae_file(&mut self, path: &Path) -> Result<()> {
        let v = read_json(path)?;
        let id = v
            .get("id")
            .and_then(json_str)
            .ok_or_else(|| SdnError::Topology(format!("SAE file {} has no id", path.display())))?;
        // Either explicit dkms_id, or dkms_target {ip,port}.
        let dkms_id = if let Some(did) = v.get("dkms_id").and_then(json_str) {
            if !self.dkms.contains_key(&did) {
                return Err(SdnError::Topology(format!(
                    "SAE {id} → DKMS {did} not registered"
                )));
            }
            did
        } else if let Some(t) = v.get("dkms_target") {
            let ip = t.get("ip").and_then(Value::as_str);
            let port = t
                .get("port")
                .and_then(Value::as_i64)
                .and_then(|n| u16::try_from(n).ok());
            let (ip, port) = ip
                .zip(port)
                .ok_or_else(|| SdnError::Topology(format!("SAE {id} dkms_target invalid")))?;
            self.dkms
                .values()
                .find(|d| d.host.ip == ip && d.host.port == port)
                .map(|d| d.id.clone())
                .ok_or_else(|| {
                    SdnError::Topology(format!(
                        "SAE {id} dkms_target {ip}:{port} did not match any DKMS"
                    ))
                })?
        } else {
            return Err(SdnError::Topology(format!(
                "SAE {id} has no dkms_id nor dkms_target"
            )));
        };
        self.upsert_sae(Sae { id, dkms_id });
        Ok(())
    }
}

// ----------------------------------------------------------------- store

/// Lock-free reader / serialized-writer wrapper around [`Topology`].
///
/// Cloning the store is cheap (it's just an `Arc`); the inner snapshot is
/// only cloned on write.
#[derive(Clone)]
pub struct TopologyStore {
    inner: Arc<ArcSwap<Topology>>,
    write_mux: Arc<Mutex<()>>,
}

impl Default for TopologyStore {
    fn default() -> Self {
        Self::new(Topology::default())
    }
}

impl TopologyStore {
    pub fn new(initial: Topology) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(initial)),
            write_mux: Arc::new(Mutex::new(())),
        }
    }

    /// Cheap, lock-free read of the current snapshot.
    pub fn load(&self) -> Arc<Topology> {
        self.inner.load_full()
    }

    /// Atomically replace the snapshot.
    pub fn replace(&self, mut new: Topology) {
        let _g = self.write_mux.lock();
        new.version = self.inner.load().version + 1;
        self.inner.store(Arc::new(new));
    }

    /// Mutate-in-place semantics on a clone of the current snapshot. If `f`
    /// returns `true`, the snapshot is published and the version bumped.
    pub fn mutate<F>(&self, f: F) -> bool
    where
        F: FnOnce(&mut Topology) -> bool,
    {
        let _g = self.write_mux.lock();
        let mut next = (**self.inner.load()).clone();
        if !f(&mut next) {
            return false;
        }
        next.version += 1;
        self.inner.store(Arc::new(next));
        true
    }

    /// Try-mutate: run `f` against a fresh clone; on `Ok(value)` publish and
    /// bump the version, on `Err` discard the clone. The returned value is
    /// whatever `f` produced.
    pub fn try_mutate<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Topology) -> Result<T>,
    {
        let _g = self.write_mux.lock();
        let mut next = (**self.inner.load()).clone();
        let out = f(&mut next)?;
        next.version += 1;
        self.inner.store(Arc::new(next));
        Ok(out)
    }

    // ----- SAE CRUD (mirrors Python SDN semantics)

    /// Register a SAE binding. Errors:
    /// * `SdnError::SaeAlreadyRegistered` if `sae_id` already exists.
    /// * `SdnError::UnknownDkms` if the requested DKMS can't be located.
    /// * `SdnError::BadRequest` if neither id nor target is supplied.
    pub fn register_sae(
        &self,
        sae_id: &str,
        dkms_id: Option<&str>,
        dkms_target: Option<(&str, u16)>,
    ) -> Result<Sae> {
        if dkms_id.is_none() && dkms_target.is_none() {
            return Err(SdnError::BadRequest(
                "must specify dkms_id or dkms_target".into(),
            ));
        }
        self.try_mutate(|t| {
            if t.saes.contains_key(sae_id) {
                return Err(SdnError::SaeAlreadyRegistered(sae_id.to_string()));
            }
            let dkms = t
                .resolve_dkms(dkms_id, dkms_target)
                .ok_or_else(|| {
                    SdnError::UnknownDkms(dkms_id.map(String::from).unwrap_or_else(|| {
                        dkms_target
                            .map(|(ip, p)| format!("{ip}:{p}"))
                            .unwrap_or_default()
                    }))
                })?
                .id
                .clone();
            let sae = Sae {
                id: sae_id.into(),
                dkms_id: dkms,
            };
            t.saes.insert(sae.id.clone(), sae.clone());
            Ok(sae)
        })
    }

    /// Register many SAEs in a single mutation (single write_mux lock,
    /// single topology clone, single atomic swap). Drastically faster
    /// than calling `register_sae` N times because the clone cost
    /// dominates for N > 100. Each item returns its own Result —
    /// failures (already registered, unknown DKMS) do not abort the
    /// batch; they appear as Err in the returned Vec.
    ///
    /// 2026-05-20: added to fix the loadtest bottleneck where
    /// concurrent POSTs to /sae stalled under the global write_mux
    /// lock + per-call topology clone.
    pub fn register_sae_bulk(&self, items: Vec<SaeBulkItem>) -> Result<Vec<BulkSaeOutcome>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut results: Vec<BulkSaeOutcome> = Vec::with_capacity(items.len());
        // ONE mutex take, ONE clone for the whole batch.
        self.try_mutate(|t| {
            for item in items.iter() {
                if item.dkms_id.is_none() && item.dkms_target.is_none() {
                    results.push(BulkSaeOutcome::Error {
                        sae_id: item.sae_id.clone(),
                        code: "bad_request".into(),
                        detail: "must specify dkms_id or dkms_target".into(),
                    });
                    continue;
                }
                if t.saes.contains_key(&item.sae_id) {
                    results.push(BulkSaeOutcome::Error {
                        sae_id: item.sae_id.clone(),
                        code: "already_registered".into(),
                        detail: format!("sae already registered: {}", item.sae_id),
                    });
                    continue;
                }
                let resolved = t.resolve_dkms(
                    item.dkms_id.as_deref(),
                    item.dkms_target.as_ref().map(|(ip, p)| (ip.as_str(), *p)),
                );
                let dkms_id = match resolved {
                    Some(d) => d.id.clone(),
                    None => {
                        let key = item.dkms_id.clone().unwrap_or_else(|| {
                            item.dkms_target
                                .as_ref()
                                .map(|(ip, p)| format!("{ip}:{p}"))
                                .unwrap_or_default()
                        });
                        results.push(BulkSaeOutcome::Error {
                            sae_id: item.sae_id.clone(),
                            code: "unknown_dkms".into(),
                            detail: format!("unknown dkms: {key}"),
                        });
                        continue;
                    }
                };
                let sae = Sae {
                    id: item.sae_id.clone(),
                    dkms_id,
                };
                t.saes.insert(sae.id.clone(), sae.clone());
                results.push(BulkSaeOutcome::Ok(sae));
            }
            Ok(())
        })?;
        Ok(results)
    }

    /// Replace the DKMS binding of an existing SAE.
    pub fn update_sae(
        &self,
        sae_id: &str,
        dkms_id: Option<&str>,
        dkms_target: Option<(&str, u16)>,
    ) -> Result<Sae> {
        if dkms_id.is_none() && dkms_target.is_none() {
            return Err(SdnError::BadRequest(
                "must specify dkms_id or dkms_target".into(),
            ));
        }
        self.try_mutate(|t| {
            if !t.saes.contains_key(sae_id) {
                return Err(SdnError::UnknownSae(sae_id.into()));
            }
            let dkms = t
                .resolve_dkms(dkms_id, dkms_target)
                .ok_or_else(|| {
                    SdnError::UnknownDkms(dkms_id.map(String::from).unwrap_or_else(|| {
                        dkms_target
                            .map(|(ip, p)| format!("{ip}:{p}"))
                            .unwrap_or_default()
                    }))
                })?
                .id
                .clone();
            let sae = Sae {
                id: sae_id.into(),
                dkms_id: dkms,
            };
            t.saes.insert(sae.id.clone(), sae.clone());
            Ok(sae)
        })
    }

    pub fn delete_sae(&self, sae_id: &str) -> Result<()> {
        self.try_mutate(|t| {
            if t.saes.remove(sae_id).is_some() {
                Ok(())
            } else {
                Err(SdnError::UnknownSae(sae_id.into()))
            }
        })
    }

    // ----- link capacity (paper §4.3 trigger 2)

    /// Apply a new capacity (keys/s) on a QKC↔QKC link. Returns
    /// `Ok(true)` if it changed, `Ok(false)` if below the deadband.
    pub fn update_edge_capacity_kps(
        &self,
        qkc_a: &str,
        qkc_b: &str,
        capacity_kps: f64,
    ) -> Result<bool> {
        if !capacity_kps.is_finite() || capacity_kps < 0.0 {
            return Err(SdnError::BadRequest("capacity must be >= 0".into()));
        }
        // We need to distinguish "not changed" from "no such edge"; do the
        // check on the current snapshot first so we don't bump version for
        // nothing.
        {
            let snap = self.load();
            if snap.edge(qkc_a, qkc_b).is_none() {
                return Err(SdnError::UnknownLink(format!("{qkc_a}<->{qkc_b}")));
            }
        }
        let mut applied = false;
        self.mutate(
            |t| match t.set_edge_capacity_kps(qkc_a, qkc_b, capacity_kps) {
                Some(true) => {
                    applied = true;
                    true
                }
                _ => false,
            },
        );
        Ok(applied)
    }

    // ----- QKC / ORR / DKMS CRUD (strict — error on missing or duplicate)
    //
    // The `upsert_*` methods on `Topology` are lenient (warn-and-skip on FK
    // misses) so the loader can absorb partially-broken config folders.
    // These store-level wrappers are stricter: they map duplicate / missing
    // to typed errors that the HTTP layer turns into 404 / 409.

    pub fn register_qkc(&self, qkc: Qkc) -> Result<Qkc> {
        self.try_mutate(|t| {
            if t.qkcs.contains_key(&qkc.id) {
                return Err(SdnError::AlreadyExists(format!("QKC {}", qkc.id)));
            }
            let id = qkc.id.clone();
            t.qkcs.insert(id.clone(), qkc.clone());
            t.graph.entry(id).or_default();
            Ok(qkc)
        })
    }

    pub fn update_qkc(&self, qkc: Qkc) -> Result<Qkc> {
        self.try_mutate(|t| {
            if !t.qkcs.contains_key(&qkc.id) {
                return Err(SdnError::UnknownNode(qkc.id.clone()));
            }
            t.qkcs.insert(qkc.id.clone(), qkc.clone());
            Ok(qkc)
        })
    }

    pub fn delete_qkc(&self, qkc_id: &str) -> Result<()> {
        self.try_mutate(|t| {
            if t.qkcs.remove(qkc_id).is_none() {
                return Err(SdnError::UnknownNode(qkc_id.into()));
            }
            // Cascade: drop the node from the graph and any edge that
            // touched it. ORR/DKMS bound to this QKC are left dangling
            // (matching Python behaviour — caller is responsible for
            // cleaning up dependents).
            t.graph.remove(qkc_id);
            for adj in t.graph.values_mut() {
                adj.remove(qkc_id);
            }
            t.edges.retain(|(a, b), _| a != qkc_id && b != qkc_id);
            t.orr_by_qkc.remove(qkc_id);
            t.dkms_by_qkc.remove(qkc_id);
            Ok(())
        })
    }

    pub fn register_orr(&self, orr: Orr) -> Result<Orr> {
        self.try_mutate(|t| {
            if !t.qkcs.contains_key(&orr.qkc_id) {
                return Err(SdnError::UnknownNode(orr.qkc_id.clone()));
            }
            if t.orrs.contains_key(&orr.id) {
                return Err(SdnError::AlreadyExists(format!("ORR {}", orr.id)));
            }
            t.orrs.insert(orr.id.clone(), orr.clone());
            t.orr_by_qkc.insert(orr.qkc_id.clone(), orr.id.clone());
            Ok(orr)
        })
    }

    pub fn update_orr(&self, orr: Orr) -> Result<Orr> {
        self.try_mutate(|t| {
            if !t.orrs.contains_key(&orr.id) {
                return Err(SdnError::Topology(format!("ORR {} not found", orr.id)));
            }
            if !t.qkcs.contains_key(&orr.qkc_id) {
                return Err(SdnError::UnknownNode(orr.qkc_id.clone()));
            }
            // If the qkc_id changed, fix the by_qkc index for both the
            // old and the new mapping.
            let old_qkc = t.orrs.get(&orr.id).map(|o| o.qkc_id.clone());
            if let Some(old) = old_qkc {
                if old != orr.qkc_id
                    && t.orr_by_qkc.get(&old).map(String::as_str) == Some(orr.id.as_str())
                {
                    t.orr_by_qkc.remove(&old);
                }
            }
            t.orr_by_qkc.insert(orr.qkc_id.clone(), orr.id.clone());
            t.orrs.insert(orr.id.clone(), orr.clone());
            Ok(orr)
        })
    }

    pub fn delete_orr(&self, orr_id: &str) -> Result<()> {
        self.try_mutate(|t| {
            let orr = t
                .orrs
                .remove(orr_id)
                .ok_or_else(|| SdnError::Topology(format!("ORR {orr_id} not found")))?;
            if t.orr_by_qkc.get(&orr.qkc_id).map(String::as_str) == Some(orr_id) {
                t.orr_by_qkc.remove(&orr.qkc_id);
            }
            Ok(())
        })
    }

    pub fn register_dkms(&self, dkms: Dkms) -> Result<Dkms> {
        self.try_mutate(|t| {
            if !t.orrs.contains_key(&dkms.orr_id) {
                return Err(SdnError::Topology(format!(
                    "DKMS {} references unknown ORR {}",
                    dkms.id, dkms.orr_id
                )));
            }
            if t.dkms.contains_key(&dkms.id) {
                return Err(SdnError::AlreadyExists(format!("DKMS {}", dkms.id)));
            }
            let qkc_id = t.orrs[&dkms.orr_id].qkc_id.clone();
            t.dkms.insert(dkms.id.clone(), dkms.clone());
            t.dkms_by_qkc.insert(qkc_id, dkms.id.clone());
            Ok(dkms)
        })
    }

    pub fn update_dkms(&self, dkms: Dkms) -> Result<Dkms> {
        self.try_mutate(|t| {
            if !t.dkms.contains_key(&dkms.id) {
                return Err(SdnError::UnknownDkms(dkms.id.clone()));
            }
            if !t.orrs.contains_key(&dkms.orr_id) {
                return Err(SdnError::Topology(format!(
                    "DKMS {} references unknown ORR {}",
                    dkms.id, dkms.orr_id
                )));
            }
            // Keep dkms_by_qkc consistent if the anchor ORR moved.
            let old_qkc = t
                .dkms
                .get(&dkms.id)
                .and_then(|d| t.orrs.get(&d.orr_id))
                .map(|o| o.qkc_id.clone());
            let new_qkc = t.orrs[&dkms.orr_id].qkc_id.clone();
            if let Some(old) = old_qkc {
                if old != new_qkc
                    && t.dkms_by_qkc.get(&old).map(String::as_str) == Some(dkms.id.as_str())
                {
                    t.dkms_by_qkc.remove(&old);
                }
            }
            t.dkms_by_qkc.insert(new_qkc, dkms.id.clone());
            t.dkms.insert(dkms.id.clone(), dkms.clone());
            Ok(dkms)
        })
    }

    pub fn delete_dkms(&self, dkms_id: &str) -> Result<()> {
        self.try_mutate(|t| {
            let dkms = t
                .dkms
                .remove(dkms_id)
                .ok_or_else(|| SdnError::UnknownDkms(dkms_id.into()))?;
            // Unlink dkms_by_qkc only if it pointed to this dkms.
            let qkc_id_opt = t.orrs.get(&dkms.orr_id).map(|o| o.qkc_id.clone());
            if let Some(qkc_id) = qkc_id_opt {
                if t.dkms_by_qkc.get(&qkc_id).map(String::as_str) == Some(dkms_id) {
                    t.dkms_by_qkc.remove(&qkc_id);
                }
            }
            Ok(())
        })
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_host(id: i64, port: u16) -> HostEndpoint {
        HostEndpoint {
            id,
            ip: format!("10.0.0.{id}"),
            port,
        }
    }

    fn make_topo() -> Topology {
        let mut t = Topology::default();
        t.upsert_qkc(Qkc {
            id: "1".into(),
            host: dummy_host(1, 9001),
            kme_host: None,
        });
        t.upsert_qkc(Qkc {
            id: "2".into(),
            host: dummy_host(2, 9002),
            kme_host: None,
        });
        t.upsert_qkc(Qkc {
            id: "3".into(),
            host: dummy_host(3, 9003),
            kme_host: None,
        });
        t.add_edge(
            "1",
            "2",
            EdgeMeta {
                distance_km: 10,
                ..EdgeMeta::default()
            },
        );
        t.add_edge(
            "2",
            "3",
            EdgeMeta {
                distance_km: 20,
                ..EdgeMeta::default()
            },
        );
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: dummy_host(11, 9101),
            qkc_id: "1".into(),
        });
        t.upsert_dkms(Dkms {
            id: "d1".into(),
            host: dummy_host(21, 9201),
            tls_id: None,
            orr_id: "o1".into(),
        });
        t.upsert_sae(Sae {
            id: "sae-a".into(),
            dkms_id: "d1".into(),
        });
        t
    }

    #[test]
    fn bfs_finds_shortest_path() {
        let t = make_topo();
        assert_eq!(
            t.shortest_path_qkc("1", "3"),
            Some(vec!["1".into(), "2".into(), "3".into()])
        );
        assert_eq!(t.next_hop_qkc("1", "3"), Some("2".into()));
    }

    #[test]
    fn quditto_capacity_decays_with_distance() {
        let near = EdgeMeta {
            distance_km: 0,
            r0_keys_per_second: 100.0,
            alpha: 0.2,
            max_buffer_size: 10,
            ..Default::default()
        };
        let far = EdgeMeta {
            distance_km: 50,
            ..near.clone()
        };
        assert!((near.quditto_capacity_keys_per_second() - 100.0).abs() < 1e-9);
        assert!(far.quditto_capacity_keys_per_second() < near.quditto_capacity_keys_per_second());
    }

    /// QKD-subgraph connectivity: `1=2 (QKD)  2~3 (PQC)  3=4 (QKD)`.
    /// The PQC edge 2~3 splits the QKD subgraph into {1,2} and {3,4}, even
    /// though the full graph is fully connected.
    fn make_mixed_topo() -> Topology {
        let mut t = Topology::default();
        for id in ["1", "2", "3", "4"] {
            t.upsert_qkc(Qkc {
                id: id.into(),
                host: dummy_host(id.parse().unwrap(), 9000 + id.parse::<u16>().unwrap()),
                kme_host: None,
            });
        }
        let qkd = |d: u32| EdgeMeta {
            distance_km: d,
            link_type: LinkType::Qkd,
            ..EdgeMeta::default()
        };
        let pqc = EdgeMeta {
            link_type: LinkType::Pqc,
            ..EdgeMeta::default()
        };
        t.add_edge("1", "2", qkd(10));
        t.add_edge("2", "3", pqc);
        t.add_edge("3", "4", qkd(10));
        t
    }

    #[test]
    fn qkd_components_split_on_pqc_edges() {
        let t = make_mixed_topo();
        let comp = t.qkd_components();
        // 1 and 2 share a QKD component; 3 and 4 share another.
        assert_eq!(comp["1"], comp["2"]);
        assert_eq!(comp["3"], comp["4"]);
        // The PQC edge does NOT join the two QKD components.
        assert_ne!(comp["1"], comp["3"]);
    }

    #[test]
    fn qkd_connectivity_ignores_pqc_links() {
        let t = make_mixed_topo();
        assert!(t.qkd_connected_qkc("1", "2"));
        assert!(t.qkd_connected_qkc("3", "4"));
        assert!(t.qkd_connected_qkc("2", "2")); // reflexive
        // 1↔3 and 1↔4 only via the PQC edge → NOT QKD-connected.
        assert!(!t.qkd_connected_qkc("1", "3"));
        assert!(!t.qkd_connected_qkc("1", "4"));
        // …but the full graph IS connected (PQC path exists).
        assert!(t.shortest_path_qkc("1", "4").is_some());
    }

    #[test]
    fn parse_edge_meta_reads_link_type() {
        use serde_json::json;
        // Ausente → QKD (backward-compatible).
        let qkd = parse_edge_meta(&json!({"distance": 5, "quditto_rate_r0": 2000.0}));
        assert_eq!(qkd.link_type, LinkType::Qkd);
        assert!(!qkd.is_pqc());
        // "pqc" (case-insensitive) → PQC.
        let pqc = parse_edge_meta(&json!({"link_type": "PQC"}));
        assert_eq!(pqc.link_type, LinkType::Pqc);
        assert!(pqc.is_pqc());
    }

    #[test]
    fn dkms_qkc_resolution() {
        let t = make_topo();
        assert_eq!(t.qkc_of_dkms("d1"), Some("1"));
    }

    #[test]
    fn register_sae_happy_path() {
        let store = TopologyStore::new(make_topo());
        let v0 = store.load().version;
        let sae = store
            .register_sae("sae-b", Some("d1"), None)
            .expect("register");
        assert_eq!(sae.dkms_id, "d1");
        assert_eq!(store.load().version, v0 + 1);
    }

    #[test]
    fn register_sae_duplicate_is_conflict() {
        let store = TopologyStore::new(make_topo());
        let err = store.register_sae("sae-a", Some("d1"), None).unwrap_err();
        assert!(matches!(err, SdnError::SaeAlreadyRegistered(_)));
    }

    #[test]
    fn register_sae_unknown_dkms_is_not_found() {
        let store = TopologyStore::new(make_topo());
        let err = store
            .register_sae("sae-x", Some("does-not-exist"), None)
            .unwrap_err();
        assert!(matches!(err, SdnError::UnknownDkms(_)));
    }

    #[test]
    fn register_sae_missing_payload_is_bad_request() {
        let store = TopologyStore::new(make_topo());
        let err = store.register_sae("sae-x", None, None).unwrap_err();
        assert!(matches!(err, SdnError::BadRequest(_)));
    }

    #[test]
    fn update_sae_rebinds_to_target_by_ip_port() {
        let mut t = make_topo();
        // Second DKMS, anchored to same ORR for simplicity.
        t.upsert_dkms(Dkms {
            id: "d2".into(),
            host: dummy_host(22, 9202),
            tls_id: None,
            orr_id: "o1".into(),
        });
        let store = TopologyStore::new(t);
        let sae = store
            .update_sae("sae-a", None, Some(("10.0.0.22", 9202)))
            .expect("update_sae");
        assert_eq!(sae.dkms_id, "d2");
    }

    #[test]
    fn delete_sae_unknown_is_not_found() {
        let store = TopologyStore::new(make_topo());
        let err = store.delete_sae("nope").unwrap_err();
        assert!(matches!(err, SdnError::UnknownSae(_)));
    }

    #[test]
    fn link_capacity_deadband_swallows_small_changes() {
        let store = TopologyStore::new(make_topo());
        let current = store
            .load()
            .edge("1", "2")
            .unwrap()
            .quditto_capacity_keys_per_second();
        // 1% delta — under both the 5% relative and 0.5 absolute thresholds.
        let tiny_change = current * 1.01_f64.min(current + 0.1);
        let changed = store
            .update_edge_capacity_kps("1", "2", tiny_change)
            .expect("ok");
        assert!(!changed, "tiny change should be absorbed by deadband");
    }

    #[test]
    fn link_capacity_unknown_link_is_not_found() {
        let store = TopologyStore::new(make_topo());
        let err = store
            .update_edge_capacity_kps("1", "99", 100.0)
            .unwrap_err();
        assert!(matches!(err, SdnError::UnknownLink(_)));
    }

    #[test]
    fn link_capacity_negative_is_bad_request() {
        let store = TopologyStore::new(make_topo());
        let err = store.update_edge_capacity_kps("1", "2", -1.0).unwrap_err();
        assert!(matches!(err, SdnError::BadRequest(_)));
    }

    #[test]
    fn register_qkc_happy_path() {
        let store = TopologyStore::new(Topology::default());
        let q = Qkc {
            id: "1".into(),
            host: dummy_host(1, 9001),
            kme_host: None,
        };
        let out = store.register_qkc(q.clone()).expect("register");
        assert_eq!(out, q);
        assert!(store.load().qkcs.contains_key("1"));
    }

    #[test]
    fn register_qkc_duplicate_is_conflict() {
        let store = TopologyStore::new(make_topo());
        let dup = Qkc {
            id: "1".into(),
            host: dummy_host(99, 9999),
            kme_host: None,
        };
        let err = store.register_qkc(dup).unwrap_err();
        assert!(matches!(err, SdnError::AlreadyExists(_)));
    }

    #[test]
    fn update_qkc_unknown_is_not_found() {
        let store = TopologyStore::new(make_topo());
        let q = Qkc {
            id: "9999".into(),
            host: dummy_host(99, 9999),
            kme_host: None,
        };
        let err = store.update_qkc(q).unwrap_err();
        assert!(matches!(err, SdnError::UnknownNode(_)));
    }

    #[test]
    fn delete_qkc_cascades_edges_and_indexes() {
        let store = TopologyStore::new(make_topo());
        store.delete_qkc("1").expect("delete");
        let snap = store.load();
        assert!(!snap.qkcs.contains_key("1"));
        // Edge "1"-"2" must be gone.
        assert!(!snap.edges.contains_key(&("1".to_string(), "2".to_string())));
        // Neighbor's adjacency cleaned up.
        assert!(!snap
            .graph
            .get("2")
            .map(|s| s.contains("1"))
            .unwrap_or(false));
    }

    #[test]
    fn register_orr_requires_existing_qkc() {
        let store = TopologyStore::new(make_topo());
        let bad = Orr {
            id: "o-x".into(),
            host: dummy_host(50, 9050),
            qkc_id: "does-not-exist".into(),
        };
        let err = store.register_orr(bad).unwrap_err();
        assert!(matches!(err, SdnError::UnknownNode(_)));
    }

    #[test]
    fn register_dkms_requires_existing_orr() {
        let store = TopologyStore::new(make_topo());
        let bad = Dkms {
            id: "d-x".into(),
            host: dummy_host(60, 9060),
            tls_id: None,
            orr_id: "no-orr".into(),
        };
        let err = store.register_dkms(bad).unwrap_err();
        assert!(matches!(err, SdnError::Topology(_)));
    }

    #[test]
    fn delete_dkms_unknown_is_not_found() {
        let store = TopologyStore::new(make_topo());
        let err = store.delete_dkms("nope").unwrap_err();
        assert!(matches!(err, SdnError::UnknownDkms(_)));
    }

    #[test]
    fn update_dkms_keeps_by_qkc_index_consistent() {
        // Build topology with 2 QKCs, 2 ORRs (one per QKC), 1 DKMS on
        // ORR-1 / QKC-1. Then re-bind the DKMS to ORR-2 (so QKC-2).
        let mut t = Topology::default();
        for id in ["q1", "q2"] {
            t.upsert_qkc(Qkc {
                id: id.into(),
                host: dummy_host(1, 1),
                kme_host: None,
            });
        }
        t.upsert_orr(Orr {
            id: "o1".into(),
            host: dummy_host(2, 2),
            qkc_id: "q1".into(),
        });
        t.upsert_orr(Orr {
            id: "o2".into(),
            host: dummy_host(3, 3),
            qkc_id: "q2".into(),
        });
        t.upsert_dkms(Dkms {
            id: "d1".into(),
            host: dummy_host(4, 4),
            tls_id: None,
            orr_id: "o1".into(),
        });
        let store = TopologyStore::new(t);

        // Move the DKMS to ORR-2.
        let moved = Dkms {
            id: "d1".into(),
            host: dummy_host(4, 4),
            tls_id: None,
            orr_id: "o2".into(),
        };
        store.update_dkms(moved).expect("update");

        let snap = store.load();
        assert_eq!(snap.dkms["d1"].orr_id, "o2");
        // dkms_by_qkc now points at q2; the q1 entry has been cleared.
        assert_eq!(snap.dkms_by_qkc.get("q2").map(String::as_str), Some("d1"));
        assert!(!snap.dkms_by_qkc.contains_key("q1"));
    }

    #[test]
    fn store_mutate_publishes_version() {
        let store = TopologyStore::new(make_topo());
        let v0 = store.load().version;
        let ok = store.mutate(|t| {
            t.upsert_sae(Sae {
                id: "sae-b".into(),
                dkms_id: "d1".into(),
            })
        });
        assert!(ok);
        assert_eq!(store.load().version, v0 + 1);
        assert!(store.load().saes.contains_key("sae-b"));
    }
}
