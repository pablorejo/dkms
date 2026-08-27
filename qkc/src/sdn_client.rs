//! Anuncio del QKC a la SDN (`POST {sdn_url}/register/qkc`).
//!
//! La SDN no arranca con una topología escrita a mano: la infiere de lo que los
//! módulos le cuentan. El QKC es la base de esa pirámide — sabe quién es, dónde
//! se le alcanza y con qué vecinos tiene enlace, y eso es exactamente lo que la
//! SDN necesita para montar el grafo. Los ORR y DKMS se cuelgan luego de él.
//!
//! **Es un bucle, no un disparo único**, por dos motivos:
//!
//! 1. Una arista necesita sus dos extremos registrados (`Topology::add_edge`),
//!    así que el QKC que arranca primero se queda con el enlace pendiente hasta
//!    que su vecino aparezca.
//! 2. La SDN puede estar caída o reiniciarse; el reanuncio la repuebla sola.
//!
//! El mismo bucle hace de heartbeat: la SDN expira los nodos que dejan de
//! anunciarse. Por eso el reanuncio es **idempotente** en el otro lado — un
//! anuncio sin cambios no toca la versión de la topología y no dispara ni el
//! push de forwarding ni el LP.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::{
    default_pqc_suite, default_rekey_keys, default_rekey_lookahead, default_rekey_secs, LinkConfig,
    LinkType, QkcConfig,
};
use crate::service::QkcService;

/// Puerto del listener TCP-peer, extraído de `peer_listen` (que suele bindear
/// `0.0.0.0`, del que solo sirve el puerto).
fn peer_port(peer_listen: &str) -> u16 {
    peer_listen
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

/// Cadencia de reintento mientras quede algo por converger (SDN inalcanzable o
/// aristas pendientes de que arranque el vecino). Una vez todo encaja se pasa a
/// `sdn_announce_secs`.
const BOOTSTRAP_RETRY: Duration = Duration::from_secs(2);

/// Lo que la SDN responde a un anuncio. Solo nos interesa para loguear: qué
/// aristas quedaron pendientes y cuáles chocan con las del vecino.
#[derive(Debug, Default, Deserialize)]
struct AnnounceOutcome {
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    edges_added: Vec<String>,
    #[serde(default)]
    edges_pending: Vec<String>,
    #[serde(default)]
    edges_conflict: Vec<String>,
    /// Con quién debe tener enlace, según la SDN. Incluye vecinos que este QKC
    /// no declaró: es lo que permite que un nodo nuevo aparezca sin
    /// reconfigurar a los que ya estaban.
    #[serde(default)]
    peers: Vec<QkcPeerWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct QkcPeerWire {
    qkc_id: String,
    peer_addr: String,
    link_type: String,
    #[serde(default = "default_key_size")]
    key_size_bits: u32,
}

fn default_key_size() -> u32 {
    256
}

#[derive(Debug, Serialize)]
struct LinkAnnounce {
    neighbor_id: String,
    link_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    r0_keys_per_second: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    alpha: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    distance_km: Option<u32>,
    // El nombre del wire es el del campo de `EdgeMeta` en la SDN (que va
    // `#[serde(flatten)]` en su parser de announce), no el de nuestro TOML.
    #[serde(skip_serializing_if = "Option::is_none")]
    pqc_capacity_keys_per_s: Option<f64>,
}

pub struct SdnAnnouncer {
    http: reqwest::Client,
    url: String,
    body: serde_json::Value,
    period: Duration,
    /// Donde se dan de alta y de baja los enlaces.
    svc: QkcService,
    /// Plantilla de la que se copian los ajustes PQC de un enlace nuevo: la
    /// SDN dice con quién hablar, no con qué suite ni cada cuánto rotar.
    link_defaults: LinkConfig,
    /// Vecinos declarados en el `node.yml`. **Nunca se retiran.** La SDN puede
    /// añadir enlaces y quitar los que ella misma añadió, pero no los de la
    /// config local: mientras no conozca todavía a un vecino su lista llega
    /// vacía, y borrarlos tiraría abajo enlaces vivos con su material de clave.
    local_links: std::collections::HashSet<u32>,
    /// Config local COMPLETA por vecino. La `node.yml` es la autoridad sobre la
    /// identidad del enlace (claves de firma/PSK, suite, capacidad); la SDN solo
    /// sabe direcciones. Sin esto, un enlace declarado localmente pero sin
    /// `neighbor_addr` —lo normal en PQC, donde la dirección la pone la SDN— se
    /// montaba por la vía de la SDN y perdía su material de firma.
    local_cfgs: std::collections::HashMap<u32, LinkConfig>,
}

impl SdnAnnouncer {
    /// `None` si el QKC no tiene `sdn_url` — es opcional: sin él, el operador
    /// da de alta el nodo a mano y todo lo demás funciona igual.
    ///
    /// Devuelve `None` **con un error logueado** si hay `sdn_url` pero no se
    /// puede deducir una IP alcanzable: preferimos que el QKC siga sirviendo
    /// claves y que el fallo se vea en el log, a no arrancar.
    pub fn from_config(cfg: &QkcConfig, svc: QkcService) -> Option<Self> {
        let sdn_url = cfg.sdn_url.as_deref()?.trim_end_matches('/').to_string();

        // El puerto sale de admin_http (es donde la SDN empuja el forwarding);
        // la IP no, porque admin_http suele ser 0.0.0.0.
        let port: u16 = cfg
            .admin_http
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let bind_ip = cfg
            .admin_http
            .rsplit_once(':')
            .map(|(h, _)| h)
            .unwrap_or("");
        let ip = match cfg.advertise_ip.as_deref() {
            Some(ip) => ip.to_string(),
            None if !bind_ip.is_empty() && bind_ip != "0.0.0.0" && bind_ip != "[::]" => {
                bind_ip.to_string()
            }
            None => {
                error!(
                    admin_http = %cfg.admin_http,
                    "sdn_url configurado pero no sé con qué IP anunciarme: admin_http bindea a \
                     todas las interfaces. Pon `advertise_ip` con la IP que la SDN pueda alcanzar. \
                     Sigo sin anunciarme.",
                );
                return None;
            }
        };

        let links: Vec<LinkAnnounce> = cfg
            .links
            .iter()
            .map(|l| {
                if l.link_type == LinkType::Qkd && l.r0.is_none() {
                    warn!(
                        neighbor = l.neighbor_id,
                        "enlace QKD sin `r0`: la SDN dimensionará la arista con su default, que \
                         es mucho más bajo que cualquier enlace real. Declara r0/alpha/distance_km",
                    );
                }
                LinkAnnounce {
                    neighbor_id: l.neighbor_id.to_string(),
                    link_type: match l.link_type {
                        LinkType::Qkd => "qkd",
                        LinkType::Pqc => "pqc",
                    },
                    r0_keys_per_second: l.r0,
                    alpha: l.alpha,
                    distance_km: l.distance_km,
                    pqc_capacity_keys_per_s: l.capacity_keys_per_s,
                }
            })
            .collect();

        let http = match common::http::announcer_client(
            &sdn_url,
            cfg.tls.as_ref().map(|t| t.as_client_tls()),
            Duration::from_secs(5),
        ) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "no pude construir el cliente de anuncio (TLS?); no me anuncio");
                return None;
            }
        };

        Some(Self {
            http,
            url: format!("{sdn_url}/register/qkc"),
            body: json!({
                "id": cfg.qkc_id.to_string(),
                "host": { "id": cfg.qkc_id as i64, "ip": ip, "port": port },
                // Los vecinos se conectan al listener de peers, no al admin:
                // la SDN necesita esta dirección para poder decirle a otro QKC
                // dónde estoy.
                "peer_addr": format!("{ip}:{}", peer_port(&cfg.peer_listen)),
                "links": links,
            }),
            period: Duration::from_secs(cfg.sdn_announce_secs.max(1)),
            svc,
            local_links: cfg.links.iter().map(|l| l.neighbor_id).collect(),
            local_cfgs: cfg.links.iter().map(|l| (l.neighbor_id, l.clone())).collect(),
            // Si el node.yml declara enlaces, sus ajustes PQC son la
            // referencia local; si no, los defaults del propio config.
            link_defaults: cfg.links.first().cloned().unwrap_or_else(|| LinkConfig {
                neighbor_id: 0,
                neighbor_peer_addr: String::new(),
                link_type: LinkType::Pqc,
                quditto_url: None,
                pqc_suite: default_pqc_suite(),
                key_size_bits: 256,
                pqc_rekey_keys: default_rekey_keys(),
                pqc_rekey_secs: default_rekey_secs(),
                pqc_rekey_lookahead: default_rekey_lookahead(),
                r0: None,
                alpha: None,
                distance_km: None,
                capacity_keys_per_s: None,
                link_psk: None,
                pqc_auth: crate::config::PqcAuth::Off,
                peer_verify_key: None,
            }),
        })
    }

    /// Anuncia una vez. `Ok(outcome)` incluso si quedan aristas pendientes:
    /// eso no es un fallo, es que el vecino aún no ha arrancado.
    async fn announce_once(&self) -> Result<AnnounceOutcome, reqwest::Error> {
        self.http
            .post(&self.url)
            .json(&self.body)
            .send()
            .await?
            .error_for_status()?
            .json::<AnnounceOutcome>()
            .await
    }

    /// Aplica los enlaces que manda la SDN.
    ///
    /// Solo se crean enlaces **PQC**: uno QKD necesita un `kme_url` que apunta
    /// al KME de esta institución, y eso la SDN no lo sabe ni puede inventarlo.
    /// Los QKD siguen viniendo del `node.yml`.
    fn apply_peers(&self, peers: &[QkcPeerWire]) {
        let wanted: std::collections::HashSet<u32> =
            peers.iter().filter_map(|p| p.qkc_id.parse().ok()).collect();

        for p in peers {
            let Ok(id) = p.qkc_id.parse::<u32>() else {
                warn!(qkc = %p.qkc_id, "id de vecino no numérico; lo ignoro");
                continue;
            };
            if p.link_type != "pqc" {
                // Un enlace QKD anunciado por la SDN se ignora si no lo
                // teníamos ya: sin `kme_url` no se puede levantar.
                if self.svc.link_to(id).is_none() {
                    warn!(
                        peer = id,
                        "la SDN anuncia un enlace QKD que no tengo configurado; hace falta su \
                         kme_url en el node.yml, la SDN no puede saberlo"
                    );
                }
                continue;
            }
            // Aquí se levanta también el vecino que el `node.yml` declaró
            // solo por id: sin dirección no se montó en el arranque, así que
            // llega como ausente y se crea ahora con el `peer_addr` que dice
            // la SDN — la única que lo sabe de todos, porque cada QKC anuncia
            // el suyo. Un enlace que ya existe no se toca: rehacerlo tiraría
            // su `SecretStore` y con él el material vivo.
            if self.svc.link_to(id).is_some() {
                continue;
            }
            // Si el enlace está declarado en el `node.yml`, esa config manda:
            // la SDN solo aporta la DIRECCIÓN. Un enlace PQC declarado sin
            // `neighbor_addr` (lo normal: la dirección la da la SDN) llegaba
            // aquí y se reconstruía desde `link_defaults`, perdiendo su
            // `peer_verify_key` — el peer firmaba y este extremo descartaba por
            // "sin peer_verify_key" (medido 2026-08-27: 302 descartes en un
            // nodo de una malla de 4).
            if let Some(local) = self.local_cfgs.get(&id) {
                let mut cfg = local.clone();
                cfg.neighbor_peer_addr = p.peer_addr.clone();
                match self.svc.add_link(cfg) {
                    Ok(true) => info!(peer = id, addr = %p.peer_addr,
                        "enlace local montado con la dirección de la SDN"),
                    Ok(false) => {}
                    Err(e) => warn!(peer = id, error = %e, "no pude levantar el enlace local"),
                }
                continue;
            }
            let cfg = LinkConfig {
                neighbor_id: id,
                neighbor_peer_addr: p.peer_addr.clone(),
                link_type: LinkType::Pqc,
                quditto_url: None,
                key_size_bits: p.key_size_bits,
                // El material de identidad NO se hereda de `link_defaults`:
                // es POR PAR. Heredarlo metía en el enlace nuevo la clave del
                // primer vecino declarado, y el peer firmaba con otra — medido
                // 2026-08-27: 436 descartes por "firma ML-DSA inválida" en una
                // malla donde la mitad de los enlaces los crea la SDN. Con un
                // PSK el efecto habría sido el mismo. La SDN no reparte
                // identidades (ver `docs/SECURITY.md`), así que un enlace
                // firmado tiene que declararse en el `node.yml` de los dos
                // extremos, como ya pasa con `kme_url` en los QKD.
                link_psk: None,
                peer_verify_key: None,
                ..self.link_defaults.clone()
            };
            if cfg.pqc_auth != crate::config::PqcAuth::Off {
                warn!(
                    peer = id,
                    modo = ?cfg.pqc_auth,
                    "enlace creado por la SDN sin material de firma: declara el enlace en el \
                     node.yml de este QKC (con peer_verify_key/link_psk) o el handshake se \
                     descartará"
                );
            }
            match self.svc.add_link(cfg) {
                Ok(true) => info!(peer = id, addr = %p.peer_addr, "enlace nuevo, dicho por la SDN"),
                Ok(false) => {}
                Err(e) => warn!(peer = id, error = %e, "no pude levantar el enlace"),
            }
        }

        // Bajas: solo lo que la SDN añadió y ya no lista. Los enlaces del
        // node.yml se quedan pase lo que pase.
        let mine: Vec<u32> = self.svc.links.load().keys().copied().collect();
        for id in mine {
            if !wanted.contains(&id) && !self.local_links.contains(&id) {
                self.svc.remove_link(id);
            }
        }
    }

    /// Bucle de anuncio. No termina nunca; va en su propia task.
    ///
    /// Mientras algo no converge reintenta deprisa, pero con backoff hasta el
    /// periodo normal: un enlace declarado contra un vecino que nunca arranca
    /// es un error de configuración, y no debe costar un POST cada dos
    /// segundos para siempre. Por lo mismo solo se loguea cuando el resultado
    /// **cambia**, no en cada vuelta.
    pub async fn run(self) {
        let mut backoff = BOOTSTRAP_RETRY;
        let mut last: Option<(Vec<String>, Vec<String>)> = None;
        loop {
            let converged = match self.announce_once().await {
                Ok(out) => {
                    let now = (out.edges_pending.clone(), out.edges_conflict.clone());
                    let is_new = last.as_ref() != Some(&now);
                    if is_new && !out.edges_conflict.is_empty() {
                        warn!(
                            url = %self.url, conflict = ?out.edges_conflict,
                            "la SDN ya tenía estas aristas con otros metadatos y mantiene los \
                             suyos: r0/alpha/distance_km no coinciden con los del vecino",
                        );
                    }
                    if out.changed || is_new {
                        info!(
                            url = %self.url, added = ?out.edges_added,
                            pending = ?out.edges_pending,
                            "anunciado a la SDN",
                        );
                    }
                    self.apply_peers(&out.peers);
                    last = Some(now);
                    out.edges_pending.is_empty()
                }
                Err(e) => {
                    if last.is_some() {
                        warn!(url = %self.url, error = %e, "perdí a la SDN; reintento");
                        last = None;
                    }
                    false
                }
            };
            let wait = if converged {
                backoff = BOOTSTRAP_RETRY;
                self.period
            } else {
                let w = backoff;
                backoff = (backoff * 2).min(self.period);
                w
            };
            tokio::time::sleep(wait).await;
        }
    }
}
