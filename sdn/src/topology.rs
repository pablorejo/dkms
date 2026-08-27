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
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    sync::Arc,
};

use arc_swap::ArcSwap;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::warn;

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
    /// Admin HTTP: por aquí la SDN le empuja su tabla de forwarding.
    pub host: HostEndpoint,
    /// `ip:port` de su listener TCP-peer. **No** se deduce de `host`: ese es
    /// el admin. Los vecinos se conectan aquí, así que la SDN lo necesita para
    /// poder decirle a uno dónde está el otro.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_addr: Option<String>,
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
    /// Puerto SAE (ETSI-014): por aquí le piden claves sus SAEs.
    pub host: HostEndpoint,
    /// `ip:port` de su listener DKMS↔DKMS (ETSI-020). Distinto de `host`, que
    /// es el de SAEs; los peers se conectan aquí.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_addr: Option<String>,
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

/// One link as announced by a QKC registering itself (see
/// [`TopologyStore::register_qkc`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QkcLinkAnnounce {
    pub neighbor_id: String,
    #[serde(flatten)]
    pub meta: EdgeMeta,
}

/// Self-announcement of a QKC: who it is, where to reach it, and which
/// neighbours it has a link with.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QkcAnnounce {
    pub id: String,
    pub host: HostEndpoint,
    /// `ip:port` de su listener TCP-peer, que es donde se conectan los vecinos
    /// — `host` es el admin. Sin esto la SDN no puede decirle a un QKC dónde
    /// está el otro.
    #[serde(default)]
    pub peer_addr: Option<String>,
    #[serde(default)]
    pub links: Vec<QkcLinkAnnounce>,
}

/// What [`TopologyStore::register_qkc`] did with an announcement. `pending`
/// and `conflict` are not errors: the QKC re-announces periodically, so the
/// caller just needs to know the edge is not in the graph yet.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QkcAnnounceOutcome {
    pub qkc_id: String,
    /// Whether the topology changed (and therefore the version was bumped).
    pub changed: bool,
    /// Edges now in the graph because of this announcement.
    pub edges_added: Vec<String>,
    /// Neighbours that have not registered yet; the edge waits for them.
    pub edges_pending: Vec<String>,
    /// Neighbours whose edge already exists with *different* metadata. The
    /// existing value is kept — see `register_qkc` for why.
    pub edges_conflict: Vec<String>,
    /// Vecinos cuya arista se ha retirado porque este anuncio dejó de
    /// declararla y el otro extremo tampoco la declara.
    #[serde(default)]
    pub edges_removed: Vec<String>,
    /// Con quién debe levantar enlace, según el grafo. Incluye los que este
    /// QKC no declaró: es lo que permite que un nodo nuevo aparezca sin
    /// reconfigurar a los que ya estaban.
    #[serde(default)]
    pub peers: Vec<QkcPeer>,
}

/// Self-announcement of an ORR: who it is, where to reach it, and which QKC it
/// hangs off.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrrAnnounce {
    pub id: String,
    pub host: HostEndpoint,
    pub qkc_id: String,
}

/// Self-announcement of a DKMS. Anchored to an ORR, which is in turn anchored
/// to a QKC — that chain is how the SDN places it in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DkmsAnnounce {
    pub id: String,
    pub host: HostEndpoint,
    /// `ip:port` de su listener DKMS↔DKMS (ETSI-020); `host` es el de SAEs.
    #[serde(default)]
    pub peer_addr: Option<String>,
    pub orr_id: String,
    /// SAEs atendidos por este DKMS. Los declara él porque es quien tiene sus
    /// certificados: nadie más sabe qué SAE cuelgan de dónde.
    #[serde(default)]
    pub saes: Vec<String>,
}

// ----- pares que la SDN comunica de vuelta en el anuncio
//
// Un módulo declara en su `node.yml` quién es y dónde está su QKC/ORR; a
// quién tiene que hablar se lo dice la SDN, que es la única con visión
// global. Así se despliega un nodo nuevo sin editar la config de los que ya
// estaban.
//
// Los tres conjuntos van **ordenados por id**: el módulo compara lo recibido
// con lo que tiene para decidir altas y bajas, y un orden inestable le haría
// creer que cambió en cada latido.

/// Vecino de un QKC. Lleva lo justo para levantar el enlace.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QkcPeer {
    pub qkc_id: String,
    /// `ip:port` de su listener TCP-peer.
    pub peer_addr: String,
    pub link_type: LinkType,
    pub key_size_bits: u32,
}

/// Par de un ORR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrrPeer {
    pub orr_id: String,
    pub qkc_id: String,
    /// URL gRPC completa, lista para usar.
    pub grpc_url: String,
}

/// Par de un DKMS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DkmsPeer {
    pub dkms_id: String,
    pub orr_id: String,
    /// `ip:port` de su listener ETSI-020.
    pub endpoint: String,
}

/// Result of an ORR/DKMS announcement. `accepted == false` is not an error: it
/// means the anchor (the QKC of an ORR, the ORR of a DKMS) has not registered
/// yet, so the announcer should keep retrying.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnnounceOutcome {
    pub id: String,
    pub accepted: bool,
    /// Whether the topology changed (and therefore the version was bumped).
    pub changed: bool,
    /// Id of the anchor we are still waiting for, when `accepted` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    /// Pares ORR, cuando el anuncio es de un ORR.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub orr_peers: Vec<OrrPeer>,
    /// Pares DKMS, cuando el anuncio es de un DKMS.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dkms_peers: Vec<DkmsPeer>,
}

/// Channel kind of a QKC↔QKC link.
///
/// * `Qkd` (default) — keys come from the shared quditto; capacity is the
///   distance-attenuated QKD rate (see [`EdgeMeta::quditto_capacity_keys_per_second`]).
/// * `Pqc` — keys are derived from an ML-KEM secret in the QKC; the link is
///   not QKD-rate-limited. Its capacity is `pqc_capacity_keys_per_s`
///   (configured per link, default 10 000): what bounds it is compute and
///   transport, not fibre, but a finite number is what lets the rate signal
///   mean something in PQC-only deployments — the old uncapacitated model
///   (a 1e9 sentinel in the solver) made λ and every `/rate` value garbage
///   there, measured 2026-08-02.
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
    /// QKD (default) or PQC.
    #[serde(default)]
    pub link_type: LinkType,
    /// Tamaño de la clave OTP del enlace. **Debe coincidir en ambos extremos**,
    /// así que viaja con la arista: cuando la SDN le comunica este enlace al
    /// vecino, le va este valor y no el suyo por defecto.
    #[serde(default = "default_key_size_bits")]
    pub key_size_bits: u32,
    /// Capacidad de un enlace PQC en claves/s (ignorada en QKD, donde manda
    /// la fórmula de quditto). Nadie la mide: es lo que el operador declara
    /// que su enlace puede sostener. El default (10 000) queda muy por encima
    /// del uso real por par (el techo del token bucket del DKMS es 320) y muy
    /// por debajo del viejo centinela de 1e9 que hacía la señal de rates
    /// insignificante en despliegues PQC-only.
    #[serde(default = "default_pqc_capacity")]
    pub pqc_capacity_keys_per_s: f64,
}

/// Respaldo cuando un QKC anuncia un enlace QKD sin declarar `r0`. Antes eran
/// 20.0, que servía porque el renderer del `topology.yml` siempre escribía un
/// valor explícito; ahora que la topología solo llega por anuncios, ese default
/// es el único que queda, y 20 claves/s estrangulaban el enlace sin avisar.
/// 2000 es el valor de referencia del repo (`docker/README.md`, `CLAUDE.md`).
fn default_r0() -> f64 {
    2000.0
}
fn default_alpha() -> f64 {
    0.2
}
fn default_key_size_bits() -> u32 {
    256
}
fn default_buf_size() -> u32 {
    100
}
fn default_pqc_capacity() -> f64 {
    10_000.0
}

impl Default for EdgeMeta {
    fn default() -> Self {
        Self {
            distance_km: 0,
            r0_keys_per_second: default_r0(),
            alpha: default_alpha(),
            max_buffer_size: default_buf_size(),
            link_type: LinkType::default(),
            key_size_bits: default_key_size_bits(),
            pqc_capacity_keys_per_s: default_pqc_capacity(),
        }
    }
}

impl EdgeMeta {
    /// `C_e = R₀ · 10^(−α·d/10)` (keys/s).
    pub fn quditto_capacity_keys_per_second(&self) -> f64 {
        let exp = -self.alpha * self.distance_km as f64 / 10.0;
        (self.r0_keys_per_second * 10f64.powf(exp)).max(0.0)
    }

    /// Capacidad efectiva de la arista para el modelo de rates, del tipo que
    /// sea el enlace: la fórmula de quditto en QKD, la capacidad declarada en
    /// PQC. Único punto de decisión — quien necesite la capacidad de una
    /// arista pasa por aquí, no por `is_pqc()` más un caso especial.
    pub fn capacity_keys_per_second(&self) -> f64 {
        if self.is_pqc() {
            self.pqc_capacity_keys_per_s.max(0.0)
        } else {
            self.quditto_capacity_keys_per_second()
        }
    }

    /// `true` for PQC links.
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

    /// Qué enlaces **declara** cada QKC, por declarante y vecino. No es el
    /// grafo: es lo que cada institución dice de sí misma, y el grafo se
    /// deriva de aquí.
    ///
    /// Existe para que un anuncio pueda *retirar* un enlace, no solo añadirlo.
    /// Sin esta memoria `announce_qkc` no distingue "ya no tengo fibra con
    /// aquel" de "nunca hablé de aquel", así que una arista solo desaparecía
    /// al caducar un nodo entero: recablear era imposible y la SDN llegaba a
    /// rehacerle al QKC el enlace que el operador acababa de quitarle.
    ///
    /// La arista (a,b) existe **si y solo si** `a` declara `b` **o** `b`
    /// declara `a`. Con un extremo basta, y es deliberado por los dos lados:
    /// el arranque de uno no puede borrar lo que declaró el otro, y quien
    /// retira su enlace no depende de que el vecino lo retire también.
    ///
    /// Se guarda lo que declara un QKC aunque el vecino no exista todavía —
    /// eso es la arista "pendiente"— y se conserva si el vecino se cae: sigue
    /// siendo verdad que este extremo tiene fibra hacia allí, así que cuando
    /// el otro vuelva la arista se rehace sola.
    pub declared: HashMap<String, BTreeMap<String, EdgeMeta>>,

    /// Monotonically increasing snapshot version. Bumped on every mutation.
    pub version: i64,
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

// ----------------------------------------------------------------- peers
//
// Qué le toca hablar a cada módulo, derivado del grafo. Es lo que la SDN
// devuelve en la respuesta al anuncio.

impl Topology {
    /// Vecinos de `qkc_id`: los del grafo, con su dirección de peer y los
    /// parámetros del enlace.
    ///
    /// Un vecino sin `peer_addr` se omite con un aviso: es un QKC de una
    /// versión anterior que no lo anuncia, y sin esa dirección el otro extremo
    /// no puede conectarse. Mejor omitirlo que dar un enlace inservible.
    pub fn qkc_peers(&self, qkc_id: &str) -> Vec<QkcPeer> {
        let Some(neighbors) = self.graph.get(qkc_id) else {
            return Vec::new();
        };
        let mut out: Vec<QkcPeer> = neighbors
            .iter()
            .filter_map(|n| {
                let qkc = self.qkcs.get(n)?;
                let Some(peer_addr) = qkc.peer_addr.clone() else {
                    warn!(
                        qkc = %n,
                        "vecino sin peer_addr anunciado; no puedo decirle a nadie dónde está"
                    );
                    return None;
                };
                let meta = self.edge(qkc_id, n)?;
                Some(QkcPeer {
                    qkc_id: n.clone(),
                    peer_addr,
                    link_type: meta.link_type,
                    key_size_bits: meta.key_size_bits,
                })
            })
            .collect();
        out.sort_by(|a, b| a.qkc_id.cmp(&b.qkc_id));
        out
    }

    /// Todos los demás ORR de la red. No se filtra por adyacencia: el
    /// transporte ORR es E2E (`max_hops = 1` por defecto), así que cualquier
    /// par puede necesitar hablar con cualquier otro.
    pub fn orr_peers(&self, orr_id: &str) -> Vec<OrrPeer> {
        let mut out: Vec<OrrPeer> = self
            .orrs
            .values()
            .filter(|o| o.id != orr_id)
            .map(|o| OrrPeer {
                orr_id: o.id.clone(),
                qkc_id: o.qkc_id.clone(),
                grpc_url: format!("http://{}:{}", o.host.ip, o.host.port),
            })
            .collect();
        out.sort_by(|a, b| a.orr_id.cmp(&b.orr_id));
        out
    }

    /// Todos los demás DKMS, con el ORR por el que se les llega.
    pub fn dkms_peers(&self, dkms_id: &str) -> Vec<DkmsPeer> {
        let mut out: Vec<DkmsPeer> = self
            .dkms
            .values()
            .filter(|d| d.id != dkms_id)
            .filter_map(|d| {
                let Some(endpoint) = d.peer_addr.clone() else {
                    warn!(dkms = %d.id, "peer sin peer_addr anunciado; lo omito");
                    return None;
                };
                Some(DkmsPeer {
                    dkms_id: d.id.clone(),
                    orr_id: d.orr_id.clone(),
                    endpoint,
                })
            })
            .collect();
        out.sort_by(|a, b| a.dkms_id.cmp(&b.dkms_id));
        out
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
        // Si se ha movido de QKC, el de origen se quedó sin ORR y no puede
        // seguir apuntando a este. `orr_by_qkc` es lo que `GetOrrPath` usa
        // para montar el camino ORR-level, así que una entrada rancia manda
        // el tráfico por un ORR que ya no cuelga de ahí. El camino manual
        // (`update_orr`) ya lo limpiaba; este, por donde entran TODOS los
        // anuncios, no.
        if let Some(old) = self.orrs.get(&key).map(|o| o.qkc_id.clone()) {
            if old != orr.qkc_id && self.orr_by_qkc.get(&old).map(String::as_str) == Some(&*key) {
                self.orr_by_qkc.remove(&old);
            }
        }
        // `orr_by_qkc` es 1:1 y se queda con el último. Que dos ORR cuelguen
        // del mismo QKC no lo soporta el modelo, y falla en silencio: el
        // desplazado deja de ser resoluble por su QKC, así que el material
        // sale cifrado para el ORR equivocado. Medido el 2026-08-20 al
        // re-anclar un ORR sobre un QKC ocupado — `recv_corrupt` al 100 % en
        // todo lo que enviaba un DKMS, mientras lo que recibía estaba bien y
        // la topología se veía perfecta.
        if let Some(other) = self.orr_by_qkc.get(&orr.qkc_id) {
            if other != &key {
                warn!(
                    qkc = %orr.qkc_id, displaced = %other, new = %key,
                    "two ORRs on the same QKC: the index is 1:1 and keeps the last one. The \
                     displaced ORR stops being resolvable by its QKC and its DKMS will get key \
                     material encrypted for the wrong ORR — give each QKC its own ORR",
                );
            }
        }
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
        // El índice va por el QKC del ORR del que cuelga, así que cambiar de
        // ORR puede cambiar de QKC. Igual que en `upsert_orr`: el de origen se
        // quedó sin DKMS y no puede seguir apuntándole.
        let old_qkc = self
            .dkms
            .get(&key)
            .map(|d| d.orr_id.clone())
            .and_then(|orr_id| self.orrs.get(&orr_id).map(|o| o.qkc_id.clone()));
        if let Some(old) = old_qkc {
            if old != qkc_id && self.dkms_by_qkc.get(&old).map(String::as_str) == Some(&*key) {
                self.dkms_by_qkc.remove(&old);
            }
        }
        // El índice es 1:1 por QKC: si cuelgan varios DKMS del mismo, gana el
        // último en anunciarse y el otro deja de ser resoluble por su QKC.
        // Mismo fallo silencioso que en `upsert_orr`.
        if let Some(other) = self.dkms_by_qkc.get(&qkc_id) {
            if other != &key {
                warn!(
                    qkc = %qkc_id, displaced = %other, new = %key,
                    "two DKMS resolve to the same QKC: the index is 1:1 and keeps the last one. \
                     The displaced one stops being reachable through it",
                );
            }
        }
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

    /// Sustituye **en bloque** lo que `qkc_id` declara y deja el grafo como
    /// digan las declaraciones de todos.
    ///
    /// En bloque, no incremental: el anuncio es la verdad completa sobre los
    /// enlaces de ese QKC, y es justo lo que permite retirar uno. Solo se
    /// recalculan las aristas de los vecinos implicados —los de antes y los
    /// de ahora—, así que un latido idéntico no mueve nada ni bumpea la
    /// versión, que es lo que mantiene quieto al solver.
    fn apply_declared_links(
        &mut self,
        qkc_id: &str,
        links: BTreeMap<String, EdgeMeta>,
        out: &mut QkcAnnounceOutcome,
    ) -> bool {
        let prev = self
            .declared
            .insert(qkc_id.to_string(), links.clone())
            .unwrap_or_default();
        let mut changed = prev != links;
        let mut seen = HashSet::new();
        for other in prev.keys().chain(links.keys()) {
            if other != qkc_id && seen.insert(other.clone()) {
                changed |= self.recompute_edge(qkc_id, other, out);
            }
        }
        changed
    }

    /// Deja la arista (`me`, `other`) como digan las declaraciones de sus dos
    /// extremos. `me` es quien acaba de anunciarse: es su punto de vista el
    /// que se reporta en `out`.
    fn recompute_edge(&mut self, me: &str, other: &str, out: &mut QkcAnnounceOutcome) -> bool {
        let from_me = self.declared.get(me).and_then(|m| m.get(other)).cloned();
        let from_other = self.declared.get(other).and_then(|m| m.get(me)).cloned();
        let key = edge_key(me, other);

        let meta = match (from_me, from_other) {
            // Ya no la declara ninguno de los dos: fuera del grafo. Es la
            // única vía por la que una arista desaparece sin que caduque un
            // nodo entero.
            (None, None) => return self.drop_edge(me, other, out),
            // Un solo declarante: manda él, también cuando cambia de idea.
            // Sin esto no había manera de corregir el r0 o los km de una
            // fibra en caliente: la diferencia con la arista existente se
            // tomaba por un desacuerdo con el vecino, y el vecino no había
            // abierto la boca.
            (Some(m), None) | (None, Some(m)) => m,
            (Some(mine), Some(theirs)) if mine == theirs => mine,
            // Declaran los dos y no coinciden: se conserva lo que ya había y
            // se avisa. Last-write-wins haría que dos extremos en desacuerdo
            // se pisaran en cada latido, bumpeando la versión para siempre.
            (Some(mine), Some(theirs)) => {
                out.edges_conflict.push(other.to_string());
                warn!(
                    a = me, b = other, mine = ?mine, theirs = ?theirs,
                    "link metadata disagrees between endpoints; keeping the existing value. \
                     Make both node.yml agree — the SDN sizes this edge from it",
                );
                match self.edges.get(&key) {
                    Some(_) => return false,
                    None => mine,
                }
            }
        };

        // Una arista necesita sus dos extremos registrados. Que falte uno no
        // es un error, es que todavía no ha arrancado: la declaración queda
        // guardada y la cierra el anuncio del que falta.
        if !self.qkcs.contains_key(me) || !self.qkcs.contains_key(other) {
            out.edges_pending.push(other.to_string());
            return false;
        }

        let was_there = self.graph.get(me).is_some_and(|adj| adj.contains(other));
        self.graph
            .entry(me.to_string())
            .or_default()
            .insert(other.to_string());
        self.graph
            .entry(other.to_string())
            .or_default()
            .insert(me.to_string());
        if !was_there {
            out.edges_added.push(other.to_string());
        }
        if self.edges.get(&key) == Some(&meta) {
            return !was_there;
        }
        self.edges.insert(key, meta);
        true
    }

    /// Saca la arista del grafo. Devuelve si había algo que sacar.
    fn drop_edge(&mut self, me: &str, other: &str, out: &mut QkcAnnounceOutcome) -> bool {
        let had_meta = self.edges.remove(&edge_key(me, other)).is_some();
        let had_me = self.graph.get_mut(me).is_some_and(|adj| adj.remove(other));
        let had_other = self.graph.get_mut(other).is_some_and(|adj| adj.remove(me));
        if had_meta || had_me || had_other {
            out.edges_removed.push(other.to_string());
            return true;
        }
        false
    }

    /// Cierra las aristas que otros declararon contra `qkc_id` mientras no
    /// existía.
    ///
    /// Antes una arista pendiente solo se cerraba cuando **re-anunciaba quien
    /// la declaró**, o sea hasta `sdn_announce_secs` después de que el vecino
    /// apareciera. Guardadas las declaraciones se cierra en cuanto entra el
    /// que faltaba, desde cualquiera de los dos lados.
    fn close_declarations_pointing_at(
        &mut self,
        qkc_id: &str,
        out: &mut QkcAnnounceOutcome,
    ) -> bool {
        let mine = self.declared.get(qkc_id).cloned().unwrap_or_default();
        let others: Vec<String> = self
            .declared
            .iter()
            // Las que este QKC declara también ya las miró `apply_declared_links`.
            .filter(|(d, links)| {
                d.as_str() != qkc_id && links.contains_key(qkc_id) && !mine.contains_key(*d)
            })
            .map(|(d, _)| d.clone())
            .collect();
        let mut changed = false;
        for other in others {
            changed |= self.recompute_edge(qkc_id, &other, out);
        }
        changed
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
        let current = edge.capacity_keys_per_second();
        let threshold = (current * 0.05).max(0.5);
        if (current - capacity_kps).abs() < threshold {
            return Some(false);
        }
        if edge.is_pqc() {
            // En PQC la capacidad ES el campo declarado; no hay fórmula que
            // invertir.
            edge.pqc_capacity_keys_per_s = capacity_kps;
        } else {
            let exp = edge.alpha * edge.distance_km as f64 / 10.0;
            edge.r0_keys_per_second = capacity_kps * 10f64.powf(exp);
        }
        Some(true)
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

    // ----- self-registration (auto_conf_sdn)

    /// Fold a QKC's self-announcement into the graph.
    ///
    /// El anuncio es **autoritativo sobre lo que este QKC declara**: sustituye
    /// en bloque sus enlaces anteriores, así que dejar de nombrar a un vecino
    /// retira la arista si el otro extremo tampoco la declara. Antes esto era
    /// puramente aditivo y una arista solo caía al caducar un nodo entero: no
    /// se podía recablear un enlace ni corregir su modelo físico, y la SDN
    /// llegaba a devolverle al QKC el enlace que el operador acababa de
    /// quitarle. Ver [`Topology::declared`].
    ///
    /// Idempotent on purpose: QKCs re-announce periodically as a heartbeat, so
    /// an unchanged announcement must **not** bump the version — every bump
    /// re-pushes forwarding tables and re-runs the LP.
    ///
    /// Edges need both endpoints registered, so a QKC that starts before its
    /// neighbour gets `edges_pending`; su declaración queda guardada y la
    /// cierra el anuncio del que faltaba, venga de un lado o del otro.
    /// Nothing is retried here — the QKC's announce loop is what makes it
    /// converge.
    ///
    /// On conflicting metadata the **existing** value wins and the clash is
    /// logged. Last-write-wins would be worse than useless here: two endpoints
    /// disagreeing would overwrite each other on every heartbeat, bumping the
    /// version forever and thrashing the solver. Un único declarante no entra
    /// en esa regla: no hay con quién discrepar, así que sus cambios se
    /// aplican.
    pub fn announce_qkc(&self, reg: &QkcAnnounce) -> QkcAnnounceOutcome {
        let mut out = QkcAnnounceOutcome {
            qkc_id: reg.id.clone(),
            ..Default::default()
        };
        let changed = self.mutate(|t| {
            // Keep any kme_host we already knew: overwriting it with None on
            // every heartbeat would look like a change and bump the version.
            let kme_host = t.qkcs.get(&reg.id).and_then(|q| q.kme_host.clone());
            let mut changed = t.upsert_qkc(Qkc {
                id: reg.id.clone(),
                host: reg.host.clone(),
                peer_addr: reg.peer_addr.clone(),
                kme_host,
            });
            // Lo que declara AHORA sustituye a lo que dijera antes.
            let links: BTreeMap<String, EdgeMeta> = reg
                .links
                .iter()
                .filter(|l| l.neighbor_id != reg.id)
                .map(|l| (l.neighbor_id.clone(), l.meta.clone()))
                .collect();
            changed |= t.apply_declared_links(&reg.id, links, &mut out);
            // Y este QKC puede ser el vecino que otros llevaban esperando.
            changed |= t.close_declarations_pointing_at(&reg.id, &mut out);
            changed
        });
        out.changed = changed;
        out
    }

    /// Fold an ORR's self-announcement in. Same contract as
    /// [`Self::announce_qkc`]: idempotent, and a miss on the anchor is a
    /// "not yet", not a failure — `upsert_orr` refuses an ORR whose QKC is
    /// unknown, which is exactly the boot-order case.
    pub fn announce_orr(&self, reg: &OrrAnnounce) -> AnnounceOutcome {
        let mut out = AnnounceOutcome {
            id: reg.id.clone(),
            ..Default::default()
        };
        let mut accepted = false;
        let changed = self.mutate(|t| {
            if !t.qkcs.contains_key(&reg.qkc_id) {
                return false;
            }
            accepted = true;
            // Keep the synthetic host id if we already had one, so a heartbeat
            // does not look like a change.
            let host = HostEndpoint {
                id: t.orrs.get(&reg.id).map_or(reg.host.id, |o| o.host.id),
                ..reg.host.clone()
            };
            t.upsert_orr(Orr {
                id: reg.id.clone(),
                host,
                qkc_id: reg.qkc_id.clone(),
            })
        });
        out.accepted = accepted;
        out.changed = changed;
        if !accepted {
            out.waiting_for = Some(reg.qkc_id.clone());
        }
        out
    }

    /// Fold a DKMS's self-announcement in. Waits on its ORR, which in turn
    /// waits on its QKC — the chain converges bottom-up as each layer boots.
    pub fn announce_dkms(&self, reg: &DkmsAnnounce) -> AnnounceOutcome {
        let mut out = AnnounceOutcome {
            id: reg.id.clone(),
            ..Default::default()
        };
        let mut accepted = false;
        let changed = self.mutate(|t| {
            if !t.orrs.contains_key(&reg.orr_id) {
                return false;
            }
            accepted = true;
            let existing = t.dkms.get(&reg.id);
            let host = HostEndpoint {
                id: existing.map_or(reg.host.id, |d| d.host.id),
                ..reg.host.clone()
            };
            let tls_id = existing.and_then(|d| d.tls_id);
            let mut changed = t.upsert_dkms(Dkms {
                id: reg.id.clone(),
                host,
                peer_addr: reg.peer_addr.clone(),
                tls_id,
                orr_id: reg.orr_id.clone(),
            });
            // Los SAE van después del DKMS a propósito: `upsert_sae` exige que
            // su DKMS exista, y acabamos de meterlo.
            for sae_id in &reg.saes {
                changed |= t.upsert_sae(Sae {
                    id: sae_id.clone(),
                    dkms_id: reg.id.clone(),
                });
            }
            changed
        });
        out.accepted = accepted;
        out.changed = changed;
        if !accepted {
            out.waiting_for = Some(reg.orr_id.clone());
        }
        out
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
            // Deja de declarar: se ha ido. Lo que otros declaren HACIA él se
            // conserva —sigue siendo verdad que tienen fibra hacia allí—, así
            // que si vuelve, la arista se rehace sola con su anuncio.
            t.declared.remove(qkc_id);
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
            // Cascade to its SAEs. A SAE is served by exactly one DKMS, so
            // without this the binding outlives the DKMS forever: `GET /saes`
            // keeps listing it and `GET /sae/<id>/binding` 404s on a DKMS that
            // no longer exists. Only reachable since DKMS entities started
            // expiring (see `crate::presence`) — before that they were never
            // removed at runtime.
            t.saes.retain(|_, s| s.dkms_id != dkms_id);
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
            peer_addr: None,
            kme_host: None,
        });
        t.upsert_qkc(Qkc {
            id: "2".into(),
            host: dummy_host(2, 9002),
            peer_addr: None,
            kme_host: None,
        });
        t.upsert_qkc(Qkc {
            id: "3".into(),
            host: dummy_host(3, 9003),
            peer_addr: None,
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
            peer_addr: None,
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
                peer_addr: None,
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
            peer_addr: None,
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
            peer_addr: None,
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
            peer_addr: None,
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
            peer_addr: None,
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
            peer_addr: None,
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
                peer_addr: None,
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
            peer_addr: None,
            tls_id: None,
            orr_id: "o1".into(),
        });
        let store = TopologyStore::new(t);

        // Move the DKMS to ORR-2.
        let moved = Dkms {
            id: "d1".into(),
            host: dummy_host(4, 4),
            peer_addr: None,
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

    // ----- self-registration (auto_conf_sdn)

    fn announce(id: &str, neighbors: &[&str], r0: f64) -> QkcAnnounce {
        QkcAnnounce {
            id: id.into(),
            host: dummy_host(id.parse().unwrap_or(0), 20002),
            peer_addr: Some(format!("10.0.0.{id}:20000")),
            links: neighbors
                .iter()
                .map(|n| QkcLinkAnnounce {
                    neighbor_id: (*n).into(),
                    meta: EdgeMeta {
                        distance_km: 5,
                        r0_keys_per_second: r0,
                        alpha: 0.2,
                        max_buffer_size: 65536,
                        link_type: LinkType::Pqc,
                        key_size_bits: 256,
                        ..EdgeMeta::default()
                    },
                })
                .collect(),
        }
    }

    #[test]
    fn announce_qkc_defers_edge_until_neighbor_registers() {
        let store = TopologyStore::new(Topology::default());

        // First QKC up: it knows about "2", which has not announced yet.
        let out = store.announce_qkc(&announce("1", &["2"], 2000.0));
        assert!(out.changed, "the QKC itself must land");
        assert_eq!(out.edges_pending, vec!["2".to_string()]);
        assert!(out.edges_added.is_empty());
        assert_eq!(store.load().edges.len(), 0);

        // Neighbour comes up and declares the same link: now the edge closes.
        let out = store.announce_qkc(&announce("2", &["1"], 2000.0));
        assert_eq!(out.edges_added, vec!["1".to_string()]);
        assert!(out.edges_pending.is_empty());
        assert_eq!(store.load().edges.len(), 1);
    }

    #[test]
    fn re_announcing_is_idempotent_and_does_not_bump_version() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &["2"], 2000.0));
        store.announce_qkc(&announce("2", &["1"], 2000.0));
        let settled = store.load().version;

        // The announce loop is also the heartbeat: it fires forever. Every
        // version bump re-pushes forwarding tables and re-runs the LP, so an
        // unchanged announcement must be a no-op.
        for _ in 0..10 {
            let out = store.announce_qkc(&announce("1", &["2"], 2000.0));
            assert!(!out.changed);
            let out = store.announce_qkc(&announce("2", &["1"], 2000.0));
            assert!(!out.changed);
        }
        assert_eq!(store.load().version, settled);
    }

    /// Recablear: el QKC 3 deja de tener fibra con el 2 y pasa a tenerla con
    /// el 1. Mientras el anuncio fue aditivo esto no se podía hacer — la
    /// arista vieja se quedaba y la SDN acababa devolviéndole al 3 el enlace
    /// al 2 en la lista de peers, rehaciendo lo que el operador quitó.
    #[test]
    fn dropping_a_link_from_the_announcement_retires_the_edge() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &[], 2000.0));
        store.announce_qkc(&announce("2", &[], 2000.0));
        store.announce_qkc(&announce("3", &["2"], 2000.0));
        assert!(store.load().edge("2", "3").is_some());

        let out = store.announce_qkc(&announce("3", &["1"], 2000.0));
        assert!(out.changed);
        assert_eq!(out.edges_removed, vec!["2".to_string()]);
        assert_eq!(out.edges_added, vec!["1".to_string()]);

        let t = store.load();
        assert!(t.edge("2", "3").is_none(), "la arista vieja se retira");
        assert!(t.edge("1", "3").is_some(), "y la nueva entra");
        // Y el 2 deja de ver al 3 como vecino, que es lo que impedía que el
        // recableado sobreviviera al siguiente latido.
        assert!(t.qkc_peers("2").is_empty());
        assert_eq!(t.qkc_peers("3").len(), 1);
    }

    /// Con un solo declarante no hay con quién discrepar, así que corregir el
    /// modelo físico de la fibra tiene efecto. Antes se comparaba contra la
    /// arista existente y se tomaba por un conflicto con el vecino — que no
    /// había dicho nada.
    #[test]
    fn the_only_declarer_may_change_the_link_metadata() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &["2"], 2000.0));
        store.announce_qkc(&announce("2", &[], 2000.0));
        assert_eq!(
            store.load().edge("1", "2").unwrap().r0_keys_per_second,
            2000.0
        );

        let out = store.announce_qkc(&announce("1", &["2"], 5000.0));
        assert!(out.changed);
        assert!(out.edges_conflict.is_empty(), "nadie con quien discrepar");
        assert_eq!(
            store.load().edge("1", "2").unwrap().r0_keys_per_second,
            5000.0,
        );
    }

    /// Basta con que UNO de los dos extremos declare el enlace. Si el otro lo
    /// declaraba y deja de hacerlo, la arista sigue: lo contrario haría que
    /// arrancar un QKC que no declara nada borrase lo que su vecino sí
    /// declara, que es la mitad de los arranques.
    #[test]
    fn an_edge_survives_while_either_end_still_declares_it() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &["2"], 2000.0));
        store.announce_qkc(&announce("2", &["1"], 2000.0));

        let out = store.announce_qkc(&announce("1", &[], 2000.0));
        assert!(out.edges_removed.is_empty(), "el 2 todavía la declara");
        assert!(store.load().edge("1", "2").is_some());

        // Cuando la suelta el segundo, ya sí.
        let out = store.announce_qkc(&announce("2", &[], 2000.0));
        assert_eq!(out.edges_removed, vec!["1".to_string()]);
        assert!(store.load().edge("1", "2").is_none());
    }

    /// Una arista pendiente se cierra en cuanto aparece el vecino, sin
    /// esperar a que re-anuncie quien la declaró. Antes costaba hasta un
    /// `sdn_announce_secs` de más, y solo lo cerraba el declarante.
    #[test]
    fn a_pending_edge_closes_from_the_side_that_was_missing() {
        let store = TopologyStore::new(Topology::default());
        let out = store.announce_qkc(&announce("1", &["2"], 2000.0));
        assert_eq!(out.edges_pending, vec!["2".to_string()]);
        assert!(store.load().edge("1", "2").is_none());

        // El 2 entra sin declarar nada: la declaración del 1 basta.
        let out = store.announce_qkc(&announce("2", &[], 2000.0));
        assert_eq!(out.edges_added, vec!["1".to_string()]);
        assert!(store.load().edge("1", "2").is_some());
    }

    /// Un nodo que se cae no borra lo que sus vecinos declaran hacia él:
    /// sigue siendo verdad que tienen fibra hacia allí. Cuando vuelve, la
    /// arista se rehace con su propio anuncio.
    #[test]
    fn a_neighbours_declaration_outlives_the_node_it_points_at() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &["2"], 2000.0));
        store.announce_qkc(&announce("2", &[], 2000.0));
        assert!(store.load().edge("1", "2").is_some());

        store.delete_qkc("2").expect("estaba");
        assert!(store.load().edge("1", "2").is_none(), "cascada al borrar");

        let out = store.announce_qkc(&announce("2", &[], 2000.0));
        assert_eq!(out.edges_added, vec!["1".to_string()]);
        assert!(store.load().edge("1", "2").is_some(), "vuelve sola");
    }

    #[test]
    fn conflicting_link_metadata_keeps_the_existing_value() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &["2"], 2000.0));
        store.announce_qkc(&announce("2", &["1"], 2000.0));
        let settled = store.load().version;

        // "2" está mal configurado con otro r0. Cambiar lo que declara sí es
        // un cambio de topología —queda registrado—, así que la primera vez
        // bumpea una vez. Lo que no puede es bumpear en CADA latido: eso es
        // lo que haría last-write-wins, con los dos extremos pisándose para
        // siempre y el solver recalculando sin parar.
        let first = store.announce_qkc(&announce("2", &["1"], 500.0));
        assert!(first.changed, "la declaración nueva se registra");
        assert_eq!(first.edges_conflict, vec!["1".to_string()]);
        let after_first = store.load().version;
        assert_eq!(after_first, settled + 1);

        for _ in 0..5 {
            let out = store.announce_qkc(&announce("2", &["1"], 500.0));
            assert!(!out.changed, "repetir lo mismo no cambia nada");
            assert_eq!(out.edges_conflict, vec!["1".to_string()]);
        }
        assert_eq!(store.load().version, after_first, "no bumpea por latido");

        // Y la arista conserva el valor del que llegó primero.
        let meta = store.load().edge("1", "2").cloned().unwrap();
        assert_eq!(meta.r0_keys_per_second, 2000.0);
    }

    #[test]
    fn announce_chain_converges_bottom_up_whatever_the_boot_order() {
        let store = TopologyStore::new(Topology::default());
        let orr = OrrAnnounce {
            id: "orr_1".into(),
            host: dummy_host(201, 20003),
            qkc_id: "1".into(),
        };
        let dkms = DkmsAnnounce {
            id: "dkms-1".into(),
            host: dummy_host(301, 20005),
            peer_addr: Some("10.0.0.301:20006".into()),
            orr_id: "orr_1".into(),
            saes: vec!["sae_1".into()],
        };

        // Worst case: everything boots upside down. Each layer waits for the
        // one below instead of corrupting the graph.
        let out = store.announce_dkms(&dkms);
        assert!(!out.accepted);
        assert_eq!(out.waiting_for.as_deref(), Some("orr_1"));
        let out = store.announce_orr(&orr);
        assert!(!out.accepted);
        assert_eq!(out.waiting_for.as_deref(), Some("1"));
        assert_eq!(store.load().version, 0, "nothing landed, nothing bumped");

        // QKC arrives; now the chain closes as each retry comes round.
        store.announce_qkc(&announce("1", &[], 2000.0));
        assert!(store.announce_orr(&orr).accepted);
        assert!(store.announce_dkms(&dkms).accepted);

        let t = store.load();
        assert_eq!(t.qkc_of_dkms("dkms-1"), Some("1"));
        // Los SAE del DKMS entran con él: sin esto la SDN no puede resolver
        // `sae -> DKMS` y el intercambio ETSI-014 entre nodos falla.
        assert_eq!(t.saes["sae_1"].dkms_id, "dkms-1");
    }

    /// Mover un ORR de QKC por anuncio tiene que dejar el índice inverso
    /// limpio. `update_orr` —el camino manual— ya quitaba la entrada vieja;
    /// `upsert_orr`, que es por donde entran todos los anuncios, no. El QKC
    /// que se quedó sin ORR seguía figurando con el suyo, y ese índice es lo
    /// que `GetOrrPath` usa para montar el camino ORR-level: la SDN mandaba
    /// el tráfico por un ORR que ya no cuelga de ahí.
    #[test]
    fn re_anchoring_an_orr_clears_the_index_of_the_qkc_it_left() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &[], 2000.0));
        store.announce_qkc(&announce("2", &[], 2000.0));
        store.announce_orr(&OrrAnnounce {
            id: "orr_1".into(),
            host: dummy_host(201, 20003),
            qkc_id: "1".into(),
        });
        assert_eq!(store.load().orr_by_qkc.get("1").unwrap(), "orr_1");

        let out = store.announce_orr(&OrrAnnounce {
            id: "orr_1".into(),
            host: dummy_host(201, 20003),
            qkc_id: "2".into(),
        });
        assert!(out.changed);
        let t = store.load();
        assert_eq!(t.orrs.get("orr_1").unwrap().qkc_id, "2");
        assert_eq!(t.orr_by_qkc.get("2").map(String::as_str), Some("orr_1"));
        assert!(
            !t.orr_by_qkc.contains_key("1"),
            "el qkc 1 se quedó sin ORR: no puede seguir apuntando al que se fue",
        );
    }

    /// Lo mismo para el DKMS, cuyo índice va por el QKC del ORR del que
    /// cuelga: mover el DKMS a un ORR de otro QKC dejaba el de origen
    /// apuntándole.
    #[test]
    fn re_anchoring_a_dkms_clears_the_index_of_the_qkc_it_left() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &[], 2000.0));
        store.announce_qkc(&announce("2", &[], 2000.0));
        for (orr, qkc, port) in [("orr_1", "1", 20003), ("orr_2", "2", 20103)] {
            store.announce_orr(&OrrAnnounce {
                id: orr.into(),
                host: dummy_host(201, port),
                qkc_id: qkc.into(),
            });
        }
        let dkms = |orr: &str| DkmsAnnounce {
            id: "dkms-1".into(),
            host: dummy_host(301, 20005),
            peer_addr: Some("10.0.0.301:20006".into()),
            orr_id: orr.into(),
            saes: vec![],
        };
        store.announce_dkms(&dkms("orr_1"));
        assert_eq!(store.load().dkms_by_qkc.get("1").unwrap(), "dkms-1");

        store.announce_dkms(&dkms("orr_2"));
        let t = store.load();
        assert_eq!(t.dkms_by_qkc.get("2").map(String::as_str), Some("dkms-1"));
        assert!(
            !t.dkms_by_qkc.contains_key("1"),
            "el qkc 1 se quedó sin DKMS: no puede seguir apuntándole",
        );
    }

    #[test]
    fn re_announcing_orr_and_dkms_does_not_bump_version() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &[], 2000.0));
        let orr = OrrAnnounce {
            id: "orr_1".into(),
            host: dummy_host(201, 20003),
            qkc_id: "1".into(),
        };
        let dkms = DkmsAnnounce {
            id: "dkms-1".into(),
            host: dummy_host(301, 20005),
            peer_addr: Some("10.0.0.301:20006".into()),
            orr_id: "orr_1".into(),
            saes: vec![],
        };
        store.announce_orr(&orr);
        store.announce_dkms(&dkms);
        let settled = store.load().version;

        for _ in 0..10 {
            assert!(!store.announce_orr(&orr).changed);
            assert!(!store.announce_dkms(&dkms).changed);
        }
        assert_eq!(store.load().version, settled);
    }

    // ----- pares devueltos en el anuncio (auto_conf_peers)

    /// Monta 1↔2↔3 (cadena, sin 1↔3) con su ORR y DKMS en cada nodo.
    fn chain_of_three() -> TopologyStore {
        let store = TopologyStore::new(Topology::default());
        for id in ["1", "2", "3"] {
            let mut a = announce(id, &[], 2000.0);
            a.peer_addr = Some(format!("10.0.0.{id}:20000"));
            store.announce_qkc(&a);
        }
        for (a, b) in [("1", "2"), ("2", "3")] {
            let mut an = announce(a, &[b], 2000.0);
            an.peer_addr = Some(format!("10.0.0.{a}:20000"));
            store.announce_qkc(&an);
        }
        for id in ["1", "2", "3"] {
            store.announce_orr(&OrrAnnounce {
                id: format!("orr_{id}"),
                host: dummy_host(id.parse().unwrap(), 20003),
                qkc_id: id.into(),
            });
            store.announce_dkms(&DkmsAnnounce {
                id: format!("dkms-{id}"),
                host: dummy_host(id.parse().unwrap(), 20005),
                peer_addr: Some(format!("10.0.0.{id}:20006")),
                orr_id: format!("orr_{id}"),
                saes: vec![],
            });
        }
        store
    }

    #[test]
    fn qkc_peers_are_its_graph_neighbours_with_a_reachable_address() {
        let t = chain_of_three().load();
        let p = t.qkc_peers("2");
        assert_eq!(p.len(), 2, "el 2 está en medio de la cadena");
        assert_eq!(p[0].qkc_id, "1");
        assert_eq!(p[1].qkc_id, "3");
        // La dirección es la de peer (20000), no la del admin: es donde se
        // conecta el vecino.
        assert_eq!(p[0].peer_addr, "10.0.0.1:20000");
        // Los extremos solo ven a su único vecino.
        assert_eq!(t.qkc_peers("1").len(), 1);
    }

    #[test]
    fn a_neighbour_without_peer_addr_is_omitted_not_advertised_broken() {
        let store = chain_of_three();
        // Un QKC de una versión anterior: no anuncia peer_addr.
        let mut old = announce("3", &["2"], 2000.0);
        old.peer_addr = None;
        store.announce_qkc(&old);
        let p = store.load().qkc_peers("2");
        assert!(
            p.iter().all(|x| x.qkc_id != "3"),
            "sin dirección de peer el enlace no se puede levantar; darlo sería peor que omitirlo"
        );
    }

    #[test]
    fn orr_and_dkms_peers_exclude_self_and_carry_their_anchor() {
        let t = chain_of_three().load();
        let o = t.orr_peers("orr_2");
        assert_eq!(o.len(), 2);
        assert!(o.iter().all(|x| x.orr_id != "orr_2"));
        assert_eq!(o[0].qkc_id, "1", "el par trae el QKC del que cuelga");
        assert!(o[0].grpc_url.starts_with("http://"));

        let d = t.dkms_peers("dkms-2");
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|x| x.dkms_id != "dkms-2"));
        assert_eq!(d[0].orr_id, "orr_1");
        // Puerto ETSI-020 (20006), no el de SAEs (20005).
        assert!(d[0].endpoint.ends_with(":20006"));
    }

    #[test]
    fn peer_sets_are_stable_across_calls() {
        let t = chain_of_three().load();
        // El módulo compara lo recibido con lo que tiene para decidir altas y
        // bajas. Si el orden bailara, cada latido parecería un cambio y habría
        // altas/bajas en bucle.
        for _ in 0..10 {
            assert_eq!(t.qkc_peers("2"), t.qkc_peers("2"));
            assert_eq!(t.orr_peers("orr_2"), t.orr_peers("orr_2"));
            assert_eq!(t.dkms_peers("dkms-2"), t.dkms_peers("dkms-2"));
        }
    }

    #[test]
    fn deleting_a_dkms_takes_its_saes_with_it() {
        let store = TopologyStore::new(Topology::default());
        store.announce_qkc(&announce("1", &[], 2000.0));
        store.announce_orr(&OrrAnnounce {
            id: "orr_1".into(),
            host: dummy_host(201, 20003),
            qkc_id: "1".into(),
        });
        store.announce_dkms(&DkmsAnnounce {
            id: "dkms-1".into(),
            host: dummy_host(301, 20005),
            peer_addr: Some("10.0.0.301:20006".into()),
            orr_id: "orr_1".into(),
            saes: vec!["sae_1".into()],
        });
        assert!(store.load().saes.contains_key("sae_1"));

        // Un DKMS caducado se lleva sus SAE: si no, el binding sobrevive al
        // DKMS y `GET /sae/sae_1/binding` responde 404 sobre un DKMS que ya
        // no existe, para siempre. Detectado en el laboratorio Proxmox al
        // apagar un nodo entero.
        store.delete_dkms("dkms-1").unwrap();
        assert!(!store.load().saes.contains_key("sae_1"));
    }

    #[test]
    fn announce_qkc_ignores_self_links_and_updates_host() {
        let store = TopologyStore::new(Topology::default());
        let mut a = announce("1", &["1"], 2000.0);
        let out = store.announce_qkc(&a);
        assert!(out.edges_added.is_empty() && out.edges_pending.is_empty());
        assert_eq!(store.load().edges.len(), 0);

        // A node that moves to another IP must be picked up.
        a.host.ip = "10.9.9.9".into();
        let out = store.announce_qkc(&a);
        assert!(out.changed);
        assert_eq!(store.load().qkcs["1"].host.ip, "10.9.9.9");
    }
}
