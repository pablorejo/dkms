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

use crate::config::{LinkType, QkcConfig};

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
}

pub struct SdnAnnouncer {
    http: reqwest::Client,
    url: String,
    body: serde_json::Value,
    period: Duration,
}

impl SdnAnnouncer {
    /// `None` si el QKC no tiene `sdn_url` — es opcional: sin él, el operador
    /// da de alta el nodo a mano y todo lo demás funciona igual.
    ///
    /// Devuelve `None` **con un error logueado** si hay `sdn_url` pero no se
    /// puede deducir una IP alcanzable: preferimos que el QKC siga sirviendo
    /// claves y que el fallo se vea en el log, a no arrancar.
    pub fn from_config(cfg: &QkcConfig) -> Option<Self> {
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
                }
            })
            .collect();

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;

        Some(Self {
            http,
            url: format!("{sdn_url}/register/qkc"),
            body: json!({
                "id": cfg.qkc_id.to_string(),
                "host": { "id": cfg.qkc_id as i64, "ip": ip, "port": port },
                "links": links,
            }),
            period: Duration::from_secs(cfg.sdn_announce_secs.max(1)),
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
