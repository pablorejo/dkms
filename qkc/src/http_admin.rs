//! HTTP admin: lo que la SDN (o tú con `curl` durante pruebas) usa para
//! manipular la forwarding table.
//!
//! Endpoints:
//!
//! ```text
//!   GET  /healthz                                liveness
//!   GET  /forwarding-table                        snapshot actual
//!   POST /forwarding-table  body: { "replace": { "5": 2, "7": 2 } }                       # legacy single-hop
//!   POST /forwarding-table  body: { "replace": { "5": [{"qkc_id":2,"weight":3}, ...] } }  # WCMP
//!   POST /forwarding-table  body: { "updates": { "5": 2 }, "removes": [7] }
//! ```
//!
//! Phase 4 adds WCMP: each destination can carry a list of weighted
//! next hops. Old single-`u32` shape stays accepted (parsed as a
//! single-entry list with weight 1) so legacy bootstrap scripts keep
//! working.

use std::{collections::HashMap, sync::atomic::Ordering};

use axum::{
    extract::State, http::StatusCode, response::IntoResponse, routing::get, Extension, Json, Router,
};
use serde::Deserialize;
use tracing::{info, warn};

use crate::{mtls_admin::PeerCertIdentity, routing::NextHop, service::QkcService};

/// Única identidad autorizada a empujar la forwarding table (B1): el cert de
/// nodo de la SDN (SAN `dkms://sdn`), que es con el que su cliente de push se
/// presenta (`sdn/src/service.rs`, `common::http::mtls_client`). Es el nombre
/// fijo del despliegue mantenido (`render_config.py` escribe `node_id = "sdn"`
/// y `gen-certs.sh sdn <ip>` emite ese SAN).
const FORWARDING_PUSHER: &str = "sdn";

/// Autorización del `POST /forwarding-table`. Quien lo controla decide por
/// dónde viaja cada frame OTP de este nodo, así que con mTLS no basta
/// «cualquier cert de la net-ca» — un QKC/ORR/DKMS comprometido no debe poder
/// redirigir el tráfico ajeno. Sin extensión (listener en claro, el opt-out
/// [tls]-less de red interna) se mantiene el comportamiento histórico; con
/// mTLS, sólo la SDN. Los GET (tabla, stats) se quedan en el listón mTLS: son
/// diagnóstico, no mutan.
fn sdn_may_push(identity: Option<&PeerCertIdentity>) -> Result<(), String> {
    match identity {
        None => Ok(()),
        Some(id) => match id.node_id.as_deref() {
            Some(FORWARDING_PUSHER) => Ok(()),
            other => Err(format!(
                "forwarding-table: el cert del cliente ({}) no es la SDN ('{FORWARDING_PUSHER}')",
                other.unwrap_or("<sin SAN dkms://>")
            )),
        },
    }
}

pub fn router(svc: QkcService) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/forwarding-table", get(get_table).post(post_table))
        .route("/stats", get(get_stats))
        .with_state(svc)
}

/// Sirve el admin. Con `tls` (la identidad `[tls]` del nodo) el listener va
/// con mTLS y cert de cliente OBLIGATORIO contra la CA de red: por aquí entra
/// `POST /forwarding-table`, que decide por dónde viaja cada frame OTP — en
/// claro era un secuestro de rutas para cualquiera con acceso al puerto. El
/// esquema es de despliegue, como `grpc_tls`: la SDN empuja `https` sii ella
/// misma tiene `[tls]`, así que un solo lado configurado falla ruidoso en
/// ambos, no en silencio. Sin `tls` queda el claro histórico (misma máquina
/// o red interna de confianza).
pub async fn serve(
    svc: QkcService,
    addr: &str,
    tls: Option<common::http::ControlTlsCfg>,
) -> anyhow::Result<()> {
    let parsed: std::net::SocketAddr = addr.parse()?;
    let listener = common::net::bind_reuse_addr(parsed).await?;
    match tls {
        Some(t) => {
            let cfg =
                common::tls::server_config(&t.cert_path, &t.key_path, Some(&t.control_plane_ca))
                    .map_err(|e| anyhow::anyhow!("http_admin [tls]: {e}"))?;
            crate::mtls_admin::serve_mtls(listener, cfg, router(svc)).await
        }
        None => {
            info!(%addr, "qkc.http_admin.listening");
            axum::serve(listener, router(svc)).await?;
            Ok(())
        }
    }
}

async fn healthz(State(svc): State<QkcService>) -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "qkc_id": svc.qkc_id(),
        "links":  svc.cfg.links.len(),
    }))
}

async fn get_table(State(svc): State<QkcService>) -> impl IntoResponse {
    let snap = svc.routing.snapshot();
    Json(snap)
}

/// Per-entry value in the `replace` / `updates` map. Untagged so
/// either wire shape parses transparently:
///
/// * `0`             — legacy single next-hop (weight 1 implicit)
/// * `[{"qkc_id": 2, "weight": 3}, ...]`  — WCMP entry
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum NextHopValue {
    Single(u32),
    Multi(Vec<NextHop>),
}

impl NextHopValue {
    fn into_hops(self) -> Vec<NextHop> {
        match self {
            NextHopValue::Single(qkc_id) => vec![NextHop::single(qkc_id)],
            NextHopValue::Multi(hops) => hops,
        }
    }
}

#[derive(Debug, Deserialize)]
struct TableBody {
    /// Si está presente, reemplaza la tabla completa (full graph / PQC-grade).
    #[serde(default)]
    replace: Option<HashMap<String, NextHopValue>>,

    /// Si está presente, reemplaza la tabla **QKD-only** (QKD-grade frames).
    #[serde(default)]
    replace_qkd: Option<HashMap<String, NextHopValue>>,

    /// Delta: inserta/sobreescribe estas entradas.
    #[serde(default)]
    updates: Option<HashMap<String, NextHopValue>>,

    /// Delta: elimina estas entradas (claves como string).
    #[serde(default)]
    removes: Option<Vec<String>>,
}

async fn post_table(
    State(svc): State<QkcService>,
    identity: Option<Extension<PeerCertIdentity>>,
    Json(body): Json<TableBody>,
) -> impl IntoResponse {
    if let Err(msg) = sdn_may_push(identity.as_deref()) {
        warn!(%msg, "qkc.http_admin push rechazado");
        return (StatusCode::FORBIDDEN, msg).into_response();
    }
    // Soporta dos modos: replace completo (full y/o QKD) o delta.
    let mut did_replace = false;
    if let Some(replace_qkd) = body.replace_qkd {
        let parsed = match parse_map(replace_qkd) {
            Ok(m) => m,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        svc.routing.replace_qkd(parsed);
        did_replace = true;
    }
    if let Some(replace) = body.replace {
        let parsed = match parse_map(replace) {
            Ok(m) => m,
            Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
        };
        svc.routing.replace(parsed);
        did_replace = true;
    }
    if did_replace {
        return Json(serde_json::json!({
            "mode": "replace",
            "size": svc.routing.snapshot().len(),
            "qkd_size": svc.routing.snapshot_qkd().len(),
        }))
        .into_response();
    }

    let updates = body.updates.unwrap_or_default();
    let removes_raw = body.removes.unwrap_or_default();
    let updates_parsed = match parse_map(updates) {
        Ok(m) => m,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let mut removes = Vec::with_capacity(removes_raw.len());
    for k in &removes_raw {
        match k.parse::<u32>() {
            Ok(v) => removes.push(v),
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    format!("remove key {k} is not u32"),
                )
                    .into_response();
            }
        }
    }
    svc.routing.update(updates_parsed, &removes);
    Json(serde_json::json!({
        "mode": "delta",
        "size": svc.routing.snapshot().len(),
    }))
    .into_response()
}

async fn get_stats(State(svc): State<QkcService>) -> impl IntoResponse {
    let s = &svc.stats;
    let service = serde_json::json!({
        "deliver_calls": s.deliver_calls.load(Ordering::Relaxed),
        "deliver_sends_ok": s.deliver_sends_ok.load(Ordering::Relaxed),
        "deliver_drops_full": s.deliver_drops_full.load(Ordering::Relaxed),
        "deliver_drops_closed": s.deliver_drops_closed.load(Ordering::Relaxed),
        "local_send_starts": s.local_send_starts.load(Ordering::Relaxed),
        "local_send_oks": s.local_send_oks.load(Ordering::Relaxed),
        "local_send_errs": s.local_send_errs.load(Ordering::Relaxed),
        "incoming_starts": s.incoming_starts.load(Ordering::Relaxed),
        "incoming_delivered": s.incoming_delivered.load(Ordering::Relaxed),
        "incoming_forwarded": s.incoming_forwarded.load(Ordering::Relaxed),
        "incoming_errs": s.incoming_errs.load(Ordering::Relaxed),
        "intake_dropped_full": s.intake_dropped_full.load(Ordering::Relaxed),
        "local_out_registered": svc.local_out.load().len(),
    });
    let mut links = serde_json::Map::new();
    for (peer_id, link) in svc.links.load().iter() {
        let l = link.keys.levels();
        let (sent, dropped) = svc.peer_out.stats(*peer_id).unwrap_or((0, 0));
        links.insert(
            peer_id.to_string(),
            serde_json::json!({
                "enc_buffered": l.enc_buffered,
                "dec_buffered": l.dec_buffered,
                "enc_taken": l.enc_taken,
                "dec_lookups": l.dec_lookups,
                "dec_misses": l.dec_misses,
                "refills_enc": l.refills_enc,
                "refills_dec": l.refills_dec,
                "wait_enc_called": l.wait_enc_called,
                "wait_enc_succeeded": l.wait_enc_succeeded,
                "wait_enc_timeouts": l.wait_enc_timeouts,
                "wait_dec_called": l.wait_dec_called,
                "wait_dec_succeeded": l.wait_dec_succeeded,
                "wait_dec_timeouts": l.wait_dec_timeouts,
                "peer_out_sent": sent,
                "peer_out_dropped": dropped,
            }),
        );
    }
    Json(serde_json::json!({
        "qkc_id": svc.qkc_id(),
        "service": service,
        "links": links,
    }))
}

fn parse_map(raw: HashMap<String, NextHopValue>) -> Result<HashMap<u32, Vec<NextHop>>, String> {
    let mut out = HashMap::with_capacity(raw.len());
    for (k, v) in raw {
        let kk: u32 = k.parse().map_err(|_| format!("key {k} is not u32"))?;
        out.insert(kk, v.into_hops());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    /// Un [tls] que apunta a ficheros inexistentes es un error al arrancar el
    /// admin, nunca un fallback silencioso a claro.
    #[tokio::test]
    async fn serve_with_broken_tls_errors_instead_of_falling_back() {
        let svc = crate::service::QkcService::new(crate::config::QkcConfig {
            qkc_id: 1,
            peer_listen: "127.0.0.1:0".into(),
            local_listen: "127.0.0.1:0".into(),
            admin_http: "127.0.0.1:0".into(),
            sdn_url: None,
            advertise_ip: None,
            sdn_announce_secs: 30,
            tls: None,
            sign_secret_seed: None,
            links: vec![],
        })
        .unwrap();
        let tls = common::http::ControlTlsCfg {
            cert_path: "/nonexistent/qkc.crt".into(),
            key_path: "/nonexistent/qkc.key".into(),
            control_plane_ca: "/nonexistent/net-ca.crt".into(),
        };
        let err = super::serve(svc, "127.0.0.1:0", Some(tls))
            .await
            .expect_err("tls roto debe ser error");
        assert!(format!("{err:#}").contains("http_admin [tls]"));
    }

    use super::*;

    /// The legacy `{"replace": {"22": 0}}` shape used by the
    /// `demo-star` bootstrap curls must keep parsing as a
    /// single-entry WCMP table with weight 1.
    #[test]
    fn legacy_single_hop_shape_parses_to_weight_1() {
        let body: TableBody = serde_json::from_str(r#"{"replace": {"5": 2, "7": 3}}"#).unwrap();
        let r = body.replace.unwrap();
        let parsed = parse_map(r).unwrap();
        assert_eq!(parsed.len(), 2);
        let hops_5 = parsed.get(&5).unwrap();
        assert_eq!(hops_5.len(), 1);
        assert_eq!(hops_5[0].qkc_id, 2);
        assert_eq!(hops_5[0].weight, 1);
    }

    /// The new `{"replace": {"22": [{"qkc_id":0,"weight":100}, ...]}}`
    /// WCMP shape parses verbatim.
    #[test]
    fn new_wcmp_shape_parses_verbatim() {
        let body: TableBody = serde_json::from_str(
            r#"{"replace": {"5": [{"qkc_id": 2, "weight": 3}, {"qkc_id": 4, "weight": 1}]}}"#,
        )
        .unwrap();
        let parsed = parse_map(body.replace.unwrap()).unwrap();
        let hops_5 = parsed.get(&5).unwrap();
        assert_eq!(hops_5.len(), 2);
        assert_eq!(hops_5[0].qkc_id, 2);
        assert_eq!(hops_5[0].weight, 3);
        assert_eq!(hops_5[1].qkc_id, 4);
        assert_eq!(hops_5[1].weight, 1);
    }

    /// Mixed shapes in the same payload are accepted — the parser
    /// normalises per-entry, not per-payload.
    #[test]
    fn mixed_legacy_and_wcmp_in_one_payload() {
        let body: TableBody =
            serde_json::from_str(r#"{"replace": {"5": 2, "9": [{"qkc_id": 1, "weight": 4}]}}"#)
                .unwrap();
        let parsed = parse_map(body.replace.unwrap()).unwrap();
        assert_eq!(parsed[&5].len(), 1);
        assert_eq!(parsed[&5][0].weight, 1);
        assert_eq!(parsed[&9].len(), 1);
        assert_eq!(parsed[&9][0].weight, 4);
    }

    #[test]
    fn updates_field_accepts_both_shapes_too() {
        let body: TableBody = serde_json::from_str(
            r#"{"updates": {"5": 2, "9": [{"qkc_id": 1, "weight": 4}]}, "removes": ["7"]}"#,
        )
        .unwrap();
        let parsed = parse_map(body.updates.unwrap()).unwrap();
        assert_eq!(parsed[&5][0].qkc_id, 2);
        assert_eq!(parsed[&5][0].weight, 1);
        assert_eq!(parsed[&9][0].weight, 4);
        assert_eq!(body.removes, Some(vec!["7".to_string()]));
    }

    #[test]
    fn non_numeric_key_is_rejected() {
        let mut m: HashMap<String, NextHopValue> = HashMap::new();
        m.insert("not-a-number".into(), NextHopValue::Single(1));
        let err = parse_map(m).unwrap_err();
        assert!(err.contains("not-a-number"));
    }

    /// B1: el push de la tabla sólo lo autoriza el cert de la SDN. Sin
    /// extensión (listener en claro) se mantiene el histórico; un cert de
    /// red que no es la SDN —o sin SAN `dkms://`— se rechaza.
    #[test]
    fn only_the_sdn_cert_may_push() {
        let with = |id: Option<&str>| PeerCertIdentity {
            node_id: id.map(str::to_owned),
        };
        assert!(sdn_may_push(None).is_ok(), "listener en claro: histórico");
        assert!(sdn_may_push(Some(&with(Some("sdn")))).is_ok());
        let e = sdn_may_push(Some(&with(Some("qkc-2")))).unwrap_err();
        assert!(e.contains("qkc-2") && e.contains("sdn"), "{e}");
        assert!(
            sdn_may_push(Some(&with(None))).is_err(),
            "cert verificado pero sin SAN dkms:// no autoriza"
        );
    }

    fn test_svc() -> crate::service::QkcService {
        crate::service::QkcService::new(crate::config::QkcConfig {
            qkc_id: 1,
            peer_listen: "127.0.0.1:0".into(),
            local_listen: "127.0.0.1:0".into(),
            admin_http: "127.0.0.1:0".into(),
            sdn_url: None,
            advertise_ip: None,
            sdn_announce_secs: 30,
            tls: None,
            sign_secret_seed: None,
            links: vec![],
        })
        .unwrap()
    }

    /// B1, de punta a punta sobre mTLS real: el admin sirve con el cert
    /// `qkc-1` y exige cert cliente de la net-ca; el push con el cert de la
    /// SDN pasa (200) y el mismo push con el cert de otro miembro legítimo de
    /// la red (`orr_1`) recibe 403 — mTLS solo ya no basta para reescribir
    /// rutas. Los GET de diagnóstico se quedan en el listón mTLS.
    #[tokio::test]
    async fn post_table_over_mtls_requires_the_sdn_identity() {
        let dir = std::env::temp_dir().join(format!("qkc_admin_mtls_{}", std::process::id()));
        let Some(pki) = common::test_support::mldsa_test_pki(&dir, &["qkc-1", "sdn", "orr_1"])
        else {
            common::test_support::skip_or_fail("openssl sin ML-DSA (<3.5)");
            return;
        };
        // El provider PQC del proceso, como hacen los binarios: reqwest
        // (rustls) lo necesita para verificar/presentar certs ML-DSA.
        common::tls_pqc::ensure_process_default().expect("provider PQC");

        let tls_cfg =
            common::tls::server_config(&pki.cert("qkc-1"), &pki.key("qkc-1"), Some(&pki.ca_crt))
                .expect("server_config");
        let listener = common::net::bind_reuse_addr("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(crate::mtls_admin::serve_mtls(
            listener,
            tls_cfg,
            router(test_svc()),
        ));

        let client = |id: &str| {
            common::http::mtls_client(
                common::http::ClientTls {
                    ca_path: &pki.ca_crt,
                    cert_path: &pki.cert(id),
                    key_path: &pki.key(id),
                },
                std::time::Duration::from_secs(5),
            )
            .expect("mtls client")
        };
        let url = format!("https://localhost:{}/forwarding-table", addr.port());
        let body = serde_json::json!({"replace": {"5": 2}});

        let ok = client("sdn").post(&url).json(&body).send().await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK, "la SDN empuja");

        let bad = client("orr_1").post(&url).json(&body).send().await.unwrap();
        assert_eq!(
            bad.status(),
            StatusCode::FORBIDDEN,
            "otro cert de red NO reescribe rutas"
        );

        let get = client("orr_1").get(&url).send().await.unwrap();
        assert_eq!(get.status(), StatusCode::OK, "GET diagnóstico: listón mTLS");
    }
}
