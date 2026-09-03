//! HTTP admin API consumed by the web UI and the orchestrator.
//!
//! Read-only:
//!   GET    /healthz
//!   GET    /topology
//!   GET    /qkcs /orrs /dkms /saes /links
//!   GET    /sae/{sae_id}/binding
//!   GET    /sae-bindings/{dkms_id}
//!
//! Mutations (mirror Python SDN semantics):
//!   POST   /register/qkc              a QKC announces itself + its links
//!   POST   /register/orr              an ORR announces itself (anchored to a QKC)
//!   POST   /register/dkms             a DKMS announces itself (anchored to an ORR)
//!   POST   /sae                       register a SAE
//!   PUT    /sae/{sae_id}              re-bind a SAE to another DKMS
//!   DELETE /sae/{sae_id}              remove a SAE
//!   POST   /link-capacity             notify a link-capacity change
//!   POST   /paths                     compute a path
//!   POST   /demand                    DKMS reports (L_k, B_k, δ_k) per commodity
//!   GET    /demand                    inspect the demand registry
//!
//! Autenticación del plano de control (docs/SECURITY.md §Fase 3): cuando
//! `[tls]` está configurado, `http_addr` es mTLS y las rutas mutantes exigen
//! un cert de la CA de red cuyo SAN case con el id anunciado (un módulo solo
//! se anuncia a sí mismo / rebindea sus SAEs). Sin `[tls]` el SDN sirve todo
//! en claro (comportamiento histórico; el web frontend gatea a su capa).

use axum::{
    extract::{Extension, Path as AxumPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    config::SdnTlsCfg,
    demand::{CommodityDemand, DemandReport},
    error::SdnError,
    mtls::PeerCertIdentity,
    presence::Kind,
    routing,
    service::SdnService,
    topology::{Dkms, DkmsAnnounce, OrrAnnounce, QkcAnnounce, Sae, SaeBulkItem, Topology},
};

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

async fn healthz() -> Json<Health> {
    Json(Health { status: "ok" })
}

// ---------------- read-only --------------------------------------------------

async fn get_topology(State(svc): State<SdnService>) -> impl IntoResponse {
    let t = svc.topology.load();
    Json(json!({
        "version": t.version,
        "qkcs":    t.qkcs.len(),
        "orrs":    t.orrs.len(),
        "dkms":    t.dkms.len(),
        "saes":    t.saes.len(),
        "edges":   t.edges.len(),
    }))
}

async fn get_qkcs(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .qkcs
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_orrs(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .orrs
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_dkms_all(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .dkms
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

async fn get_saes(State(svc): State<SdnService>) -> impl IntoResponse {
    Json(
        svc.topology
            .load()
            .saes
            .values()
            .cloned()
            .collect::<Vec<_>>(),
    )
}

/// GET /rate/{dkms_id} — devuelve las rates per-peer asignadas por el MCF solver.
///
/// Formato:
/// ```json
/// {
///   "dkms_id": "dkms-11",
///   "version": 7,
///   "peers": {
///     "dkms-22": {"enc": 333.0, "dec": 333.0},
///     "dkms-33": {"enc": 333.0, "dec": 333.0}
///   }
/// }
/// ```
///
/// El DKMS hace polling cada N segundos (típico 5s, configurable) y usa la
/// rate como refill del token bucket per-peer del Generator.
async fn get_rate(
    AxumPath(dkms_id): AxumPath<String>,
    State(svc): State<SdnService>,
) -> impl IntoResponse {
    let snap = svc.mcf_snapshot.load();
    let topology = svc.topology.load();
    if !topology.dkms.contains_key(&dkms_id) {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": format!("dkms {dkms_id} unknown")})),
        );
    }
    // Per-destination connectivity, so the polling DKMS can resolve a
    // request's security level (`strict_qkd`/`qkd_prefer`/`no_worry`) into a
    // key grade via `common::security::SecurityLevel::resolve_grade`:
    //   * `qkd_available` — a strictly-QKD path exists to the peer (same QKD
    //     component) → QKD-grade keys are routable.
    //   * `reachable` — any path exists (full graph) → at least PQC-grade.
    // Both are topology properties (immutable until restart), independent of
    // whether the LP has produced rates yet.
    let my_qkc = topology.qkc_of_dkms(&dkms_id);
    let qkd_comp = topology.qkd_components();
    let my_rates_grade = snap.rates_by_dkms_grade.get(&dkms_id);
    let mut peers = serde_json::Map::new();
    if let Some(my_rates) = snap.rates_by_dkms.get(&dkms_id) {
        let mut grouped: std::collections::HashMap<String, (f64, f64)> =
            std::collections::HashMap::new();
        for ((peer, role), rate) in my_rates.iter() {
            let entry = grouped.entry(peer.clone()).or_insert((0.0, 0.0));
            match role {
                crate::mcf::BufferRole::EncKeys => entry.0 = *rate,
                crate::mcf::BufferRole::DecKeys => entry.1 = *rate,
            }
        }
        for (peer, (enc, dec)) in grouped {
            let peer_qkc = topology.qkc_of_dkms(&peer);
            let (qkd_available, reachable) = match (my_qkc, peer_qkc) {
                (Some(a), Some(b)) => {
                    let qkd = a == b
                        || matches!(
                            (qkd_comp.get(a), qkd_comp.get(b)),
                            (Some(x), Some(y)) if x == y
                        );
                    let reach = topology.shortest_path_qkc(a, b).is_some();
                    (qkd, reach)
                }
                _ => (false, false),
            };
            // Per-grade enc/dec so the DKMS fills its enc_qkd / enc_pqc buffers
            // at the right rate per grade.
            let mut grades = serde_json::Map::new();
            if let Some(mg) = my_rates_grade {
                for grade in [
                    common::security::KeyGrade::Qkd,
                    common::security::KeyGrade::Pqc,
                ] {
                    let g_enc = mg
                        .get(&(peer.clone(), crate::mcf::BufferRole::EncKeys, grade))
                        .copied()
                        .unwrap_or(0.0);
                    let g_dec = mg
                        .get(&(peer.clone(), crate::mcf::BufferRole::DecKeys, grade))
                        .copied()
                        .unwrap_or(0.0);
                    grades.insert(
                        grade.as_str().to_string(),
                        json!({"enc": g_enc, "dec": g_dec}),
                    );
                }
            }
            peers.insert(
                peer,
                json!({
                    "enc": enc,
                    "dec": dec,
                    "qkd_available": qkd_available,
                    "reachable": reachable,
                    "grades": grades,
                }),
            );
        }
    }
    let topo_version = topology.version;
    (
        StatusCode::OK,
        Json(json!({
            "dkms_id":          dkms_id,
            "topology_version": topo_version,
            "peers":            peers,
        })),
    )
}

async fn get_links(State(svc): State<SdnService>) -> impl IntoResponse {
    let t = svc.topology.load();
    let v: Vec<_> = t
        .edges
        .iter()
        .map(|((a, b), meta)| {
            json!({
                "a": a,
                "b": b,
                "distance_km":        meta.distance_km,
                "r0_keys_per_second": meta.r0_keys_per_second,
                "alpha":              meta.alpha,
                "max_buffer_size":    meta.max_buffer_size,
                "link_type":          if meta.is_pqc() { "pqc" } else { "qkd" },
                "capacity_keys_per_second": meta.capacity_keys_per_second(),
                // Tasa medida in situ: la combinada (min de extremos, con
                // histéresis) que usa el solver, y los reportes crudos por
                // extremo para diagnóstico.
                "measured_keys_per_s": meta.measured_keys_per_s,
                "measured_reports": svc.topology.measured.reports(a, b),
            })
        })
        .collect();
    Json(v)
}

fn binding_json(topo: &Topology, sae: &Sae) -> Result<Value, SdnError> {
    let dkms = topo
        .dkms
        .get(&sae.dkms_id)
        .ok_or_else(|| SdnError::UnknownDkms(sae.dkms_id.clone()))?;
    Ok(build_binding(topo, &sae.id, dkms))
}

fn build_binding(topo: &Topology, sae_id: &str, dkms: &Dkms) -> Value {
    let orr_id = topo.orrs.get(&dkms.orr_id).map(|o| o.id.clone());
    json!({
        "sae_id": sae_id,
        "dkms_id": dkms.id,
        "dkms_endpoint": { "id": dkms.host.id, "ip": dkms.host.ip, "port": dkms.host.port },
        "orr_id": orr_id,
    })
}

async fn get_sae_binding(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
) -> impl IntoResponse {
    let t = svc.topology.load();
    let Some(sae) = t.saes.get(&sae_id) else {
        return err_response(SdnError::UnknownSae(sae_id));
    };
    match binding_json(&t, sae) {
        Ok(v) => Json(v).into_response(),
        Err(e) => err_response(e),
    }
}

async fn list_sae_bindings(
    State(svc): State<SdnService>,
    AxumPath(dkms_id): AxumPath<String>,
) -> impl IntoResponse {
    let t = svc.topology.load();
    let Some(dkms) = t.dkms.get(&dkms_id) else {
        return err_response(SdnError::UnknownDkms(dkms_id));
    };
    // Python returns the FULL table (not filtered by dkms_id). DKMS uses it
    // as a single bootstrap call to warm its SaeBindingCache.
    let bindings: serde_json::Map<String, Value> = t
        .saes
        .iter()
        .filter_map(|(sid, s)| {
            let d = t.dkms.get(&s.dkms_id)?;
            Some((sid.clone(), build_binding(&t, sid, d)))
        })
        .collect();
    Json(json!({
        "dkms_id":  dkms.id,
        "bindings": bindings,
    }))
    .into_response()
}

// ---------------- self-registration (auto_conf_sdn) --------------------------

/// A QKC announcing itself. Idempotent: QKCs re-post this periodically as a
/// heartbeat, and an unchanged announcement leaves the topology version alone.
async fn announce_qkc(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(reg): Json<QkcAnnounce>,
) -> axum::response::Response {
    // El id de topología del QKC es numérico ("3"), pero su cert de nodo es
    // `qkc-<id>` — la convención de gen-certs (dkms-N, orr_N, qkc-N, sdn). El
    // binding compara contra el nombre del CERT: con `reg.id` a secas, todo
    // anuncio https de un QKC se rechazaba por «identity mismatch» (cazado
    // por la malla local con el plano de control en mTLS, 2026-08-31).
    if let Err(resp) = require_identity(identity.as_deref(), &format!("qkc-{}", reg.id), "qkc") {
        return resp;
    }
    if let Err(e) = validate_qkc_announce(&reg) {
        return err_response(SdnError::BadRequest(e));
    }
    let mut out = svc.topology.announce_qkc(&reg);
    // Un QKC no tiene ancla: siempre entra, así que siempre cuenta como vivo.
    svc.presence.touch(Kind::Qkc, &reg.id);
    // Con quién debe hablar. Se calcula DESPUÉS del alta para que un QKC que
    // acaba de entrar se vea a sí mismo en el grafo.
    out.peers = svc.topology.load().qkc_peers(&reg.id);
    if out.changed {
        info!(
            qkc = %out.qkc_id,
            added = ?out.edges_added,
            removed = ?out.edges_removed,
            pending = ?out.edges_pending,
            "qkc registered",
        );
    }
    (StatusCode::OK, Json(out)).into_response()
}

/// Cotas de un anuncio de QKC (auditoría 2026-09-03, C-01/C-03/C-04/C-06/
/// C-13). Todo lo que aquí entra lo decide un miembro de la federación con
/// su propio cert, y acaba en URLs que la SDN dial-a con SU identidad
/// (`host`), en direcciones que TODOS los QKC dial-an (`peer_addr`), en el
/// modelo de capacidad (`r0`/`alpha`/`distance`) y en mapas sin cota
/// (`links`, ids). Un anuncio fuera de rango se rechaza con 400.
pub const MAX_LINKS_PER_QKC: usize = 256;
const MAX_KEYS_PER_S: f64 = 1.0e9;

fn validate_host(h: &crate::topology::HostEndpoint) -> Result<(), String> {
    // `host.ip` va interpolado en la URL del push con el cert `sdn`: tiene
    // que ser una IP y nada más (C-01: `10.0.0.7:20002/forwarding-table?z=`
    // hacía que la SDN entregara SU tabla a un QKC ajeno).
    h.ip.parse::<std::net::IpAddr>().map_err(|_| {
        format!(
            "host.ip no es una IP: {:?}",
            h.ip.chars().take(64).collect::<String>()
        )
    })?;
    if h.port == 0 {
        return Err("host.port = 0".to_string());
    }
    Ok(())
}

fn validate_qkc_announce(reg: &QkcAnnounce) -> Result<(), String> {
    if !common::ids::is_valid_id(&reg.id) || reg.id.parse::<u32>().is_err() {
        return Err("qkc id debe ser numérico".to_string());
    }
    validate_host(&reg.host)?;
    if let Some(pa) = reg.peer_addr.as_deref() {
        pa.parse::<std::net::SocketAddr>().map_err(|_| {
            format!(
                "peer_addr no es ip:puerto: {:?}",
                pa.chars().take(64).collect::<String>()
            )
        })?;
    }
    if reg.links.len() > MAX_LINKS_PER_QKC {
        return Err(format!(
            "{} links, máximo {MAX_LINKS_PER_QKC}",
            reg.links.len()
        ));
    }
    let finite_in = |v: f64, lo: f64, hi: f64, what: &str| -> Result<(), String> {
        if !v.is_finite() || v < lo || v > hi {
            return Err(format!("{what} fuera de rango [{lo}, {hi}]: {v}"));
        }
        Ok(())
    };
    for l in &reg.links {
        if !common::ids::is_valid_id(&l.neighbor_id) || l.neighbor_id.parse::<u32>().is_err() {
            return Err("neighbor_id debe ser numérico".to_string());
        }
        finite_in(l.meta.r0_keys_per_second, 0.0, MAX_KEYS_PER_S, "r0")?;
        finite_in(l.meta.alpha, 0.0, 100.0, "alpha")?;
        finite_in(
            l.meta.pqc_capacity_keys_per_s,
            0.0,
            MAX_KEYS_PER_S,
            "capacity_keys_per_s",
        )?;
        if l.meta.distance_km > 100_000 {
            return Err(format!(
                "distance_km fuera de rango: {}",
                l.meta.distance_km
            ));
        }
        if let Some(m) = l.measured_rate_keys_per_s {
            finite_in(m, 0.0, MAX_KEYS_PER_S, "measured_rate_keys_per_s")?;
        }
        if let Some(q) = l.measured_quality.as_deref() {
            if q.len() > 32 {
                return Err("measured_quality demasiado largo".to_string());
            }
        }
    }
    Ok(())
}

/// An ORR announcing itself, anchored to its QKC.
async fn announce_orr(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(reg): Json<OrrAnnounce>,
) -> axum::response::Response {
    if let Err(resp) = require_identity(identity.as_deref(), &reg.id, "orr") {
        return resp;
    }
    if let Err(e) = validate_host(&reg.host).and_then(|()| {
        if !common::ids::is_valid_id(&reg.id) || reg.qkc_id.parse::<u32>().is_err() {
            Err("orr_id/qkc_id inválidos".to_string())
        } else {
            Ok(())
        }
    }) {
        return err_response(SdnError::BadRequest(e));
    }
    let mut out = svc.topology.announce_orr(&reg);
    // Solo si entró: lo que no está en la topología no puede caducar de ella.
    if out.accepted {
        svc.presence.touch(Kind::Orr, &reg.id);
        out.orr_peers = svc.topology.load().orr_peers(&reg.id);
    }
    if out.changed {
        info!(orr = %out.id, qkc = %reg.qkc_id, "orr registered");
    }
    (StatusCode::OK, Json(out)).into_response()
}

/// A DKMS announcing itself, anchored to its ORR.
async fn announce_dkms(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(reg): Json<DkmsAnnounce>,
) -> axum::response::Response {
    if let Err(resp) = require_identity(identity.as_deref(), &reg.id, "dkms") {
        return resp;
    }
    if let Err(e) = validate_host(&reg.host).and_then(|()| {
        if !common::ids::is_valid_id(&reg.id) || !common::ids::is_valid_id(&reg.orr_id) {
            return Err("dkms id/orr_id inválidos".to_string());
        }
        if let Some(pa) = reg.peer_addr.as_deref() {
            pa.parse::<std::net::SocketAddr>()
                .map_err(|_| format!("peer_addr no es ip:puerto: {pa:?}"))?;
        }
        if let Some(bad) = reg.saes.iter().find(|s| !common::ids::is_valid_id(s)) {
            return Err(format!("sae id inválido ({} bytes)", bad.len()));
        }
        Ok(())
    }) {
        return err_response(SdnError::BadRequest(e));
    }
    let mut out = svc.topology.announce_dkms(&reg);
    if out.accepted {
        svc.presence.touch(Kind::Dkms, &reg.id);
        out.dkms_peers = svc.topology.load().dkms_peers(&reg.id);
    }
    if out.changed {
        info!(dkms = %out.id, orr = %reg.orr_id, "dkms registered");
    }
    (StatusCode::OK, Json(out)).into_response()
}

// ---------------- SAE CRUD ---------------------------------------------------

#[derive(Deserialize)]
struct DkmsTargetPayload {
    ip: String,
    port: u16,
}

#[derive(Deserialize)]
struct SaeCreatePayload {
    id: String,
    #[serde(default)]
    dkms_id: Option<String>,
    #[serde(default)]
    dkms_target: Option<DkmsTargetPayload>,
}

#[derive(Deserialize)]
struct SaeUpdatePayload {
    #[serde(default)]
    dkms_id: Option<String>,
    #[serde(default)]
    dkms_target: Option<DkmsTargetPayload>,
}

async fn register_sae(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(p): Json<SaeCreatePayload>,
) -> axum::response::Response {
    if let Err(resp) = require_sae_owner(identity.as_deref(), p.dkms_id.as_deref()) {
        return resp;
    }
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc
        .topology
        .register_sae(&p.id, p.dkms_id.as_deref(), target)
    {
        Ok(sae) => (StatusCode::CREATED, Json(sae)).into_response(),
        Err(e) => err_response(e),
    }
}

/// Body item accepted by `POST /sae-bulk`. The orchestator emits a flat
/// `{id, dkms_id}` shape (see `_sdn_http_json` callers in
/// `orchestrator/api_orchestator.py`), so we deserialize that and translate
/// to the canonical `SaeBulkItem`.
#[derive(Deserialize)]
struct SaeBulkPayloadItem {
    #[serde(alias = "sae_id")]
    id: String,
    #[serde(default)]
    dkms_id: Option<String>,
    #[serde(default)]
    dkms_target: Option<DkmsTargetPayload>,
}

async fn register_sae_bulk(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(items): Json<Vec<SaeBulkPayloadItem>>,
) -> axum::response::Response {
    // Autorización (auditoría 2026-09b A1): bajo mTLS solo puedes registrar en
    // masa SAEs que residan en TI mismo — el cert debe casar con el `dkms_id`
    // de CADA item. Antes este handler no tomaba identidad, así que era el
    // segundo paso del robo de binding (DELETE ajeno + re-registro a mi nombre).
    if identity.is_some() {
        for it in &items {
            if let Err(resp) = require_sae_owner(identity.as_deref(), it.dkms_id.as_deref()) {
                return resp;
            }
        }
    }
    let bulk: Vec<SaeBulkItem> = items
        .into_iter()
        .map(|p| SaeBulkItem {
            sae_id: p.id,
            dkms_id: p.dkms_id,
            dkms_target: p.dkms_target.map(|t| (t.ip, t.port)),
        })
        .collect();
    match svc.topology.register_sae_bulk(bulk) {
        Ok(outcomes) => (StatusCode::OK, Json(outcomes)).into_response(),
        Err(e) => err_response(e),
    }
}

async fn update_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(p): Json<SaeUpdatePayload>,
) -> axum::response::Response {
    // Autorización (auditoría 2026-09b A1): solo el dueño ACTUAL puede
    // rebindear/mover su SAE. Antes se comprobaba contra el `dkms_id` DESTINO
    // del cuerpo, así que un cert ajeno robaba el binding con
    // `PUT /sae/X {"dkms_id":"yo"}` y recibía la clave de sesión de X.
    let owner = svc
        .topology
        .load()
        .saes
        .get(&sae_id)
        .map(|s| s.dkms_id.clone());
    if let Err(resp) = sae_mutation_authorized(identity.as_deref(), owner.as_deref()) {
        return resp;
    }
    let target = p.dkms_target.as_ref().map(|t| (t.ip.as_str(), t.port));
    match svc
        .topology
        .update_sae(&sae_id, p.dkms_id.as_deref(), target)
    {
        Ok(sae) => Json(sae).into_response(),
        Err(e) => err_response(e),
    }
}

async fn delete_sae(
    State(svc): State<SdnService>,
    AxumPath(sae_id): AxumPath<String>,
    identity: Option<Extension<PeerCertIdentity>>,
) -> axum::response::Response {
    // Autorización (auditoría 2026-09b A1): solo el DKMS que ACTUALMENTE sirve
    // el SAE puede borrarlo. Antes no tomaba identidad — cualquiera lo borraba,
    // primer paso del robo de binding.
    let owner = svc
        .topology
        .load()
        .saes
        .get(&sae_id)
        .map(|s| s.dkms_id.clone());
    if let Err(resp) = sae_mutation_authorized(identity.as_deref(), owner.as_deref()) {
        return resp;
    }
    match svc.topology.delete_sae(&sae_id) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- link capacity (paper §4.3 trigger 2) -----------------------

#[derive(Deserialize)]
struct LinkCapacityPayload {
    qkc_a: String,
    qkc_b: String,
    capacity_keys_per_second: f64,
}

async fn update_link_capacity(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(p): Json<LinkCapacityPayload>,
) -> impl IntoResponse {
    if p.qkc_a.is_empty() || p.qkc_b.is_empty() {
        return err_response(SdnError::BadRequest("qkc_a and qkc_b are required".into()));
    }
    // Autorización (auditoría 2026-09b A2): bajo mTLS solo un extremo del enlace
    // o la SDN puede fijar su capacidad; antes cualquier cert ponía a 0 la
    // capacidad de cualquier arista (la sacaba del solver). La tasa medida ya
    // viaja en el anuncio del QKC.
    if let Err(resp) = link_capacity_authorized(identity.as_deref(), &p.qkc_a, &p.qkc_b) {
        return resp;
    }
    match svc
        .topology
        .update_edge_capacity_kps(&p.qkc_a, &p.qkc_b, p.capacity_keys_per_second)
    {
        Ok(changed) => Json(json!({
            "qkc_a": p.qkc_a,
            "qkc_b": p.qkc_b,
            "capacity_keys_per_second": p.capacity_keys_per_second,
            "changed": changed,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- path compute -----------------------------------------------

#[derive(Deserialize)]
struct PathRequest {
    src: String,
    dst: String,
    #[serde(default)]
    required_bps: u64,
    #[serde(default = "default_policy")]
    policy: String,
}
fn default_policy() -> String {
    "shortest_hops".into()
}

async fn compute_path(
    State(svc): State<SdnService>,
    Json(req): Json<PathRequest>,
) -> impl IntoResponse {
    let policy = match req.policy.as_str() {
        "min_latency" => routing::Policy::MinLatency,
        "max_available_capacity" => routing::Policy::MaxAvailableCapacity,
        "min_cost_flow" => routing::Policy::MinCostFlow,
        _ => routing::Policy::ShortestHops,
    };
    match routing::compute(&svc.topology, &req.src, &req.dst, req.required_bps, policy) {
        Ok(p) => Json(json!({
            "path": p.nodes,
            "estimated_latency_us": p.estimated_latency_us,
            "bottleneck_capacity_bps": p.bottleneck_capacity_bps,
        }))
        .into_response(),
        Err(e) => err_response(e),
    }
}

// ---------------- demand (MCMCF-λ) -------------------------------------------

/// POST /demand — a DKMS reports the current `(L_k, B_k, δ_k)` for
/// every commodity it sources. Replaces the previous report for
/// matching `(src_dkms, dst_dkms)` keys.
///
/// Body:
/// ```json
/// {
///   "dkms_id": "dkms-11",
///   "entries": [
///     {
///       "src_dkms": "dkms-11", "dst_dkms": "dkms-22",
///       "level": 1234.0, "capacity": 65536.0,
///       "drain_rate": 50.0, "timestamp_ms": 1234567890
///     }
///   ]
/// }
/// ```
///
/// Returns `{accepted: N, errors: [...]}`. Status `200` if everything
/// landed, `206` (partial content) if some entries were rejected.
///
/// Phase 1 — storage only: the MCMCF-λ solver in phase 3 will pick
/// up these reports. The legacy MCF solver is unaffected.
async fn post_demand(
    State(svc): State<SdnService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(report): Json<DemandReport>,
) -> impl IntoResponse {
    // Observes on Drop, so it covers both the bad-request early-return
    // and the happy path.
    let _timer = svc.sdn_metrics.demand_post_duration_seconds.start_timer();
    if report.dkms_id.is_empty() {
        svc.sdn_metrics
            .demand_post_total
            .with_label_values(&["bad_request"])
            .inc();
        return err_response(SdnError::BadRequest("dkms_id is required".into()));
    }
    // Autorización (auditoría 2026-09b A2): bajo mTLS un DKMS solo reporta su
    // PROPIA demanda; antes no tomaba identidad y cualquier cert inyectaba
    // demanda por cualquier `dkms_id`, sesgando el reparto alfa-fair.
    if let Err(resp) = require_identity(identity.as_deref(), &report.dkms_id, "demand") {
        return resp;
    }
    let summary = svc.demand_registry.ingest(report);
    let outcome = if summary.errors.is_empty() {
        "ok"
    } else {
        "partial"
    };
    svc.sdn_metrics
        .demand_post_total
        .with_label_values(&[outcome])
        .inc();
    let status = if summary.errors.is_empty() {
        StatusCode::OK
    } else {
        StatusCode::PARTIAL_CONTENT
    };
    (
        status,
        Json(json!({
            "accepted": summary.accepted,
            "errors":   summary.errors,
            "registry_len": svc.demand_registry.len(),
        })),
    )
        .into_response()
}

/// GET /demand — dump every recorded `(src_dkms, dst_dkms)` demand
/// snapshot. Read-only inspection endpoint for dashboards / debugging.
async fn get_demand(State(svc): State<SdnService>) -> impl IntoResponse {
    let entries: Vec<CommodityDemand> = svc.demand_registry.snapshot();
    Json(json!({ "entries": entries, "len": entries.len() }))
}

/// GET /wcmp — dump the currently-published WCMP table per
/// (transit_qkc, dst_qkc) pair. Read-only inspection of what the LP
/// fed to the forwarding push loop. The shape mirrors
/// `McfSnapshot::wcmp`: `qkc → dst → [{qkc_id, weight}, ...]`.
async fn get_wcmp(State(svc): State<SdnService>) -> impl IntoResponse {
    let snap = svc.mcf_snapshot.load();
    Json(json!({
        "wcmp": &snap.wcmp,
        "n_qkcs": snap.wcmp.len(),
    }))
}

// ---------------- error mapping ----------------------------------------------

fn err_response(e: SdnError) -> axum::response::Response {
    let (code, msg) = http_status_for(&e);
    (code, Json(json!({ "detail": msg }))).into_response()
}

fn http_status_for(e: &SdnError) -> (StatusCode, String) {
    use SdnError::*;
    let msg = e.to_string();
    let code = match e {
        UnknownNode(_) | UnknownLink(_) | UnknownDkms(_) | UnknownSae(_) | NoPath(..) => {
            StatusCode::NOT_FOUND
        }
        SaeAlreadyRegistered(_) => StatusCode::CONFLICT,
        BadRequest(_) | Topology(_) | AdmissionDenied(_) => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (code, msg)
}

// ---------------- identity binding ------------------------------------------

/// Normaliza un SAN de nodo a su id: quita los prefijos de esquema que emite
/// `docker/gen-certs.sh` (`dkms://<id>`, `urn:dkms:node:<id>`) y se queda con
/// el último segmento de una URI con path. Un SAN ya pelado se devuelve igual.
fn node_id_from_san(san: &str) -> &str {
    for prefix in ["urn:dkms:node:", "dkms://"] {
        if let Some(rest) = san.strip_prefix(prefix) {
            return rest.rsplit('/').next().unwrap_or(rest);
        }
    }
    san
}

/// ¿El cert del peer (si lo hay) autoriza a actuar como `claimed_id`?
///
/// - `None` (plano en claro, sin mTLS): se permite — preserva el
///   comportamiento histórico donde el registro no estaba autenticado.
/// - `Some` con SAN que casa `claimed_id`: permitido.
/// - `Some` con SAN distinto o ausente: denegado.
fn identity_authorizes(identity: Option<&PeerCertIdentity>, claimed_id: &str) -> bool {
    match identity.and_then(|i| i.san.as_deref()) {
        None => identity.is_none(), // sin cert (plaintext) ok; cert sin SAN no
        Some(san) => node_id_from_san(san) == claimed_id,
    }
}

/// Traduce la decisión a `Result` para los handlers. `who` es para el log.
fn require_identity(
    identity: Option<&PeerCertIdentity>,
    claimed_id: &str,
    who: &str,
) -> Result<(), axum::response::Response> {
    if identity_authorizes(identity, claimed_id) {
        return Ok(());
    }
    let san = identity.and_then(|i| i.san.clone());
    warn!(claimed = %claimed_id, cert_san = ?san, %who, "sdn: identity mismatch, rejecting");
    Err((
        StatusCode::FORBIDDEN,
        Json(json!({"detail": format!("cert identity does not authorize {who} '{claimed_id}'")})),
    )
        .into_response())
}

/// Autoriza una operación sobre un SAE: un DKMS solo puede crear/rebindear
/// SAEs que residan en sí mismo, luego el cert debe casar con `dkms_id`. En
/// mTLS exige que `dkms_id` esté presente (no se puede autorizar un bind sin
/// dueño). En claro (sin cert) se permite — comportamiento histórico.
fn require_sae_owner(
    identity: Option<&PeerCertIdentity>,
    dkms_id: Option<&str>,
) -> Result<(), axum::response::Response> {
    if identity.is_none() {
        return Ok(());
    }
    match dkms_id {
        Some(id) => require_identity(identity, id, "sae-owner"),
        None => Err((
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "mTLS: SAE bind must name its owning dkms_id"})),
        )
            .into_response()),
    }
}

/// Autoriza una MUTACIÓN (rebind/borrado) sobre un SAE existente contra su
/// dueño ACTUAL, no contra el destino del cuerpo (auditoría 2026-09b A1). Un
/// SAE inexistente (`None`) se deja pasar: el handler devuelve 404 aguas abajo.
/// En claro (sin cert) se permite — comportamiento histórico.
fn sae_mutation_authorized(
    identity: Option<&PeerCertIdentity>,
    current_owner: Option<&str>,
) -> std::result::Result<(), axum::response::Response> {
    match current_owner {
        None => Ok(()),
        Some(owner) => require_identity(identity, owner, "sae-mutation"),
    }
}

/// Autoriza `POST /link-capacity` (auditoría 2026-09b A2): bajo mTLS solo un
/// extremo del enlace (`qkc-<a>`/`qkc-<b>`) o la SDN. En claro se permite.
fn link_capacity_authorized(
    identity: Option<&PeerCertIdentity>,
    qkc_a: &str,
    qkc_b: &str,
) -> std::result::Result<(), axum::response::Response> {
    if identity.is_none()
        || identity_authorizes(identity, "sdn")
        || identity_authorizes(identity, &format!("qkc-{qkc_a}"))
        || identity_authorizes(identity, &format!("qkc-{qkc_b}"))
    {
        return Ok(());
    }
    let san = identity.and_then(|i| i.san.clone());
    warn!(qkc_a, qkc_b, cert_san = ?san, "sdn: link-capacity no autorizada");
    Err((
        StatusCode::FORBIDDEN,
        Json(json!({"detail": "cert identity does not authorize link-capacity"})),
    )
        .into_response())
}

// ---------------- server -----------------------------------------------------

/// Rutas mutantes (registro de nodos, rebind de SAE): en mTLS exigen cert.
fn mutating_routes() -> Router<SdnService> {
    Router::new()
        .route("/register/qkc", post(announce_qkc))
        .route("/register/orr", post(announce_orr))
        .route("/register/dkms", post(announce_dkms))
        .route("/sae", post(register_sae))
        .route("/sae-bulk", post(register_sae_bulk))
        .route("/sae/:sae_id", put(update_sae).delete(delete_sae))
        .route("/link-capacity", post(update_link_capacity))
        .route("/demand", post(post_demand))
        // Cota de tamaño de cuerpo (auditoría 2026-09b B11): 1 MiB cubre un
        // anuncio legítimo con miles de SAE/enlaces; frena el abuso de
        // `/sae-bulk` (por defecto axum permite 2 MiB).
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
}

/// Rutas de solo lectura (web UI / inspección). También servidas en claro
/// en `http_ro_addr` cuando el mTLS está activo.
fn readonly_routes() -> Router<SdnService> {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/topology", get(get_topology))
        .route("/qkcs", get(get_qkcs))
        .route("/orrs", get(get_orrs))
        .route("/dkms", get(get_dkms_all))
        .route("/saes", get(get_saes))
        .route("/links", get(get_links))
        .route("/sae/:sae_id/binding", get(get_sae_binding))
        .route("/sae-bindings/:dkms_id", get(list_sae_bindings))
        .route("/paths", post(compute_path))
        .route("/rate/:dkms_id", get(get_rate))
        .route("/demand", get(get_demand))
        .route("/wcmp", get(get_wcmp))
}

fn full_router(svc: SdnService) -> Router {
    mutating_routes().merge(readonly_routes()).with_state(svc)
}

pub async fn serve(svc: SdnService, addr: &str) -> anyhow::Result<()> {
    serve_with_tls(svc, addr, None, None).await
}

/// Sirve el plano HTTP. Con `tls = Some`, `addr` pasa a mTLS (rutas mutantes
/// autenticadas) y las read-only se sirven además en claro en `ro_addr`. Sin
/// `tls`, todo va en claro en `addr` (comportamiento histórico).
pub async fn serve_with_tls(
    svc: SdnService,
    addr: &str,
    tls: Option<&SdnTlsCfg>,
    ro_addr: Option<&str>,
) -> anyhow::Result<()> {
    let Some(tls) = tls else {
        let listener = TcpListener::bind(addr).await?;
        info!(%addr, "sdn HTTP listening (plaintext)");
        axum::serve(listener, full_router(svc)).await?;
        return Ok(());
    };

    let server_cfg =
        common::tls::server_config(&tls.cert_path, &tls.key_path, Some(&tls.client_ca))?;
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "sdn HTTP listening (mTLS)");
    let mtls = crate::mtls::serve_mtls(listener, server_cfg, full_router(svc.clone()));

    if let Some(ro) = ro_addr {
        let ro_listener = TcpListener::bind(ro).await?;
        info!(ro_addr = %ro, "sdn HTTP read-only listening (plaintext)");
        let ro_router = readonly_routes().with_state(svc);
        tokio::select! {
            r = mtls => r?,
            r = async move { axum::serve(ro_listener, ro_router).await } => r?,
        }
    } else {
        mtls.await?;
    }
    Ok(())
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{demand::CommodityDemand, service::tests::make_service, service::SdnService};

    fn ident(san: &str) -> PeerCertIdentity {
        PeerCertIdentity {
            san: Some(san.to_owned()),
        }
    }

    #[test]
    fn node_id_from_san_strips_schemes() {
        assert_eq!(node_id_from_san("dkms://dkms-1"), "dkms-1");
        assert_eq!(node_id_from_san("urn:dkms:node:dkms-1"), "dkms-1");
        assert_eq!(node_id_from_san("dkms://org/dkms-1"), "dkms-1");
        assert_eq!(node_id_from_san("dkms-1"), "dkms-1");
    }

    /// El id de grafo del QKC es numérico pero su cert es `qkc-<id>`: el
    /// binding del anuncio compara contra el nombre del CERT (el handler
    /// formatea `qkc-{id}`), no contra el id pelado — con el pelado, todo
    /// anuncio https de QKC se rechazaba (malla mTLS, 2026-08-31).
    #[test]
    fn a_qkc_cert_authorizes_via_its_role_prefixed_name() {
        let qkc = ident("dkms://qkc-3");
        assert!(identity_authorizes(Some(&qkc), "qkc-3"));
        assert!(
            !identity_authorizes(Some(&qkc), "3"),
            "el id de grafo a secas no es el nombre del cert"
        );
    }

    #[test]
    fn plaintext_is_allowed_but_wrong_cert_is_not() {
        // Sin cert (plano en claro): se permite — comportamiento histórico.
        assert!(identity_authorizes(None, "dkms-1"));
        // Cert cuyo SAN casa: permitido.
        assert!(identity_authorizes(Some(&ident("dkms://dkms-1")), "dkms-1"));
        // Cert de OTRO nodo intentando anunciar dkms-1: denegado.
        assert!(!identity_authorizes(
            Some(&ident("dkms://dkms-2")),
            "dkms-1"
        ));
        // Cert sin SAN: denegado.
        assert!(!identity_authorizes(
            Some(&PeerCertIdentity { san: None }),
            "dkms-1"
        ));
    }

    #[test]
    fn sae_owner_requires_matching_dkms_under_mtls() {
        // En claro: cualquier cosa pasa.
        assert!(require_sae_owner(None, Some("dkms-9")).is_ok());
        assert!(require_sae_owner(None, None).is_ok());
        // mTLS: el dkms_id del bind debe casar con el cert.
        let id = ident("dkms://dkms-1");
        assert!(require_sae_owner(Some(&id), Some("dkms-1")).is_ok());
        assert!(require_sae_owner(Some(&id), Some("dkms-2")).is_err());
        // mTLS sin dueño nombrado: denegado.
        assert!(require_sae_owner(Some(&id), None).is_err());
    }

    #[test]
    fn sae_mutation_checks_the_current_owner_not_the_body_target() {
        // En claro: se permite (histórico).
        assert!(sae_mutation_authorized(None, Some("dkms-1")).is_ok());
        // SAE inexistente: pasa (el handler devuelve 404 aguas abajo).
        assert!(sae_mutation_authorized(Some(&ident("dkms://dkms-2")), None).is_ok());
        // El dueño ACTUAL puede mutarlo.
        assert!(sae_mutation_authorized(Some(&ident("dkms://dkms-1")), Some("dkms-1")).is_ok());
        // Un cert AJENO no puede robar el binding, aunque el cuerpo pidiera
        // rebindear el SAE a su propio dkms_id: aquí solo cuenta el dueño ACTUAL.
        assert!(sae_mutation_authorized(Some(&ident("dkms://dkms-2")), Some("dkms-1")).is_err());
    }

    #[test]
    fn link_capacity_only_endpoints_or_sdn() {
        // En claro: pasa (histórico).
        assert!(link_capacity_authorized(None, "1", "2").is_ok());
        // La SDN: ok.
        assert!(link_capacity_authorized(Some(&ident("dkms://sdn")), "1", "2").is_ok());
        // Un extremo del enlace: ok.
        assert!(link_capacity_authorized(Some(&ident("dkms://qkc-1")), "1", "2").is_ok());
        assert!(link_capacity_authorized(Some(&ident("dkms://qkc-2")), "1", "2").is_ok());
        // Un QKC ajeno al enlace: denegado.
        assert!(link_capacity_authorized(Some(&ident("dkms://qkc-9")), "1", "2").is_err());
    }

    /// Bind a minimal SDN HTTP router (just the demand endpoints) to
    /// 127.0.0.1:0 and return the concrete `http://127.0.0.1:<port>`
    /// base URL. We don't reuse `serve()` directly because we want a
    /// stripped-down router that doesn't bind GRPC / metrics
    /// addresses too.
    async fn spawn_server(svc: SdnService) -> String {
        let app = Router::new()
            .route("/demand", post(post_demand).get(get_demand))
            .route("/wcmp", get(get_wcmp))
            .with_state(svc);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn post_demand_persists_entries_and_get_dumps_them() {
        let svc = make_service();
        let base = spawn_server(svc.clone()).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();

        let body = serde_json::json!({
            "dkms_id": "dA",
            "entries": [
                {
                    "src_dkms": "dA", "dst_dkms": "dB",
                    "level": 100.0, "capacity": 4096.0,
                    "drain_rate": 30.0, "timestamp_ms": 1_000,
                },
                {
                    "src_dkms": "dA", "dst_dkms": "dC",
                    "level": 200.0, "capacity": 4096.0,
                    "drain_rate": 40.0, "timestamp_ms": 1_000,
                }
            ]
        });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["accepted"], 2);
        assert_eq!(v["registry_len"], 2);

        let g = client.get(format!("{base}/demand")).send().await.unwrap();
        assert_eq!(g.status(), 200);
        let dump: serde_json::Value = g.json().await.unwrap();
        assert_eq!(dump["len"], 2);
        let entries = dump["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);

        // Round-trip through serde — make sure CommodityDemand
        // deserialises cleanly from the GET output.
        let parsed: Vec<CommodityDemand> = serde_json::from_value(dump["entries"].clone()).unwrap();
        assert!(parsed.iter().any(|e| e.dst_dkms == "dB"));
        assert!(parsed.iter().any(|e| e.dst_dkms == "dC"));
    }

    #[tokio::test]
    async fn post_demand_returns_206_when_some_entries_rejected() {
        let svc = make_service();
        let base = spawn_server(svc).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let body = serde_json::json!({
            "dkms_id": "dA",
            "entries": [
                { "src_dkms": "dA", "dst_dkms": "dB",
                  "level": 100.0, "capacity": 4096.0,
                  "drain_rate": 30.0, "timestamp_ms": 1_000 },
                // src_dkms mismatch with report dkms_id → rejected
                { "src_dkms": "dB", "dst_dkms": "dC",
                  "level": 100.0, "capacity": 4096.0,
                  "drain_rate": 30.0, "timestamp_ms": 1_000 }
            ]
        });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206);
        let v: serde_json::Value = r.json().await.unwrap();
        assert_eq!(v["accepted"], 1);
        assert_eq!(v["errors"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn post_demand_rejects_empty_dkms_id() {
        let svc = make_service();
        let base = spawn_server(svc).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let body = serde_json::json!({ "dkms_id": "", "entries": [] });
        let r = client
            .post(format!("{base}/demand"))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 400);
    }

    #[tokio::test]
    async fn post_demand_second_report_replaces_first() {
        let svc = make_service();
        let base = spawn_server(svc.clone()).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let mk = |level: f64, ts: i64| {
            serde_json::json!({
                "dkms_id": "dA",
                "entries": [{
                    "src_dkms": "dA", "dst_dkms": "dB",
                    "level": level, "capacity": 4096.0,
                    "drain_rate": 30.0, "timestamp_ms": ts,
                }]
            })
        };
        client
            .post(format!("{base}/demand"))
            .json(&mk(100.0, 1000))
            .send()
            .await
            .unwrap();
        client
            .post(format!("{base}/demand"))
            .json(&mk(500.0, 2000))
            .send()
            .await
            .unwrap();
        let g = svc
            .demand_registry
            .get("dA", "dB", common::security::KeyGrade::Qkd)
            .unwrap();
        assert_eq!(g.level, 500.0);
        assert_eq!(g.timestamp_ms, 2000);
        assert_eq!(svc.demand_registry.len(), 1);
    }
}
