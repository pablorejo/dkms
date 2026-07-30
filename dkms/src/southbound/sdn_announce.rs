//! Anuncio del DKMS a la SDN (`POST {sdn_http_url}/register/dkms`).
//!
//! Va por HTTP, como el `POST /demand` y el `GET /rate` que el DKMS ya hace
//! contra el mismo admin de la SDN (ver [`super::sdn_http`]); el gRPC de
//! `sdn_endpoint` es para el plano de control.
//!
//! El DKMS se ancla a su ORR, y ese ORR a un QKC. La SDN rechaza un DKMS cuyo
//! `orr_id` aún no conoce, así que la cadena converge de abajo arriba según
//! arranca cada capa. Por eso es un bucle con backoff, que hace además de
//! heartbeat.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::DkmsConfig;

const BOOTSTRAP_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Deserialize)]
struct Outcome {
    #[serde(default)]
    accepted: bool,
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    waiting_for: Option<String>,
}

#[derive(Debug, Serialize)]
struct Announce {
    id: String,
    host: Host,
    orr_id: String,
    saes: Vec<String>,
}

#[derive(Debug, Serialize)]
struct Host {
    id: i64,
    ip: String,
    port: u16,
}

pub struct SdnAnnouncer {
    http: reqwest::Client,
    url: String,
    body: serde_json::Value,
    period: Duration,
}

impl SdnAnnouncer {
    /// `None` si falta `southbound.sdn_http_url` u `southbound.orr_id` (el
    /// auto-registro es opcional), o si no se puede deducir una IP alcanzable
    /// — en ese caso con un error logueado, pero sin impedir el arranque.
    pub fn from_config(cfg: &DkmsConfig) -> Option<Self> {
        let base = cfg
            .southbound
            .sdn_http_url
            .as_deref()?
            .trim_end_matches('/')
            .to_string();
        let Some(orr_id) = cfg.southbound.orr_id.as_deref() else {
            error!(
                "sdn_http_url configurado pero falta `southbound.orr_id`: la SDN me coloca en el \
                 grafo colgando de mi ORR, y su dirección no vale como id. Sigo sin anunciarme.",
            );
            return None;
        };

        // La SDN alcanza al DKMS por su puerto SAE (ETSI-014).
        let port = cfg.listen.sae_addr.port();
        let bind_ip = cfg.listen.sae_addr.ip();
        let ip = match cfg.advertise_ip.as_deref() {
            Some(ip) => ip.to_string(),
            None if !bind_ip.is_unspecified() => bind_ip.to_string(),
            None => {
                error!(
                    sae_addr = %cfg.listen.sae_addr,
                    "sdn_http_url configurado pero no sé con qué IP anunciarme: sae_addr bindea a \
                     todas las interfaces. Pon `advertise_ip`. Sigo sin anunciarme.",
                );
                return None;
            }
        };

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;

        // Mis SAE: las entradas de `sae_bindings` que apuntan a mí. La SDN las
        // necesita para resolver `sae → DKMS` cuando un SAE pide claves contra
        // otro; nadie más sabe qué SAE cuelgan de aquí.
        let mut saes: Vec<String> = cfg
            .sae_bindings
            .iter()
            .filter(|(_, node)| *node == &cfg.node_id)
            .map(|(sae, _)| sae.clone())
            .collect();
        saes.sort(); // orden estable: si no, el payload cambiaría en cada boot

        Some(Self {
            http,
            url: format!("{base}/register/dkms"),
            body: json!(Announce {
                id: cfg.node_id.clone(),
                host: Host { id: 0, ip, port },
                orr_id: orr_id.to_string(),
                saes,
            }),
            period: Duration::from_secs(cfg.sdn_announce_secs.max(1)),
        })
    }

    async fn announce_once(&self) -> Result<Outcome, reqwest::Error> {
        self.http
            .post(&self.url)
            .json(&self.body)
            .send()
            .await?
            .error_for_status()?
            .json::<Outcome>()
            .await
    }

    /// Bucle de anuncio. No termina nunca; va en su propia task.
    pub async fn run(self) {
        let mut backoff = BOOTSTRAP_RETRY;
        let mut last_state: Option<(bool, Option<String>)> = None;
        loop {
            let accepted = match self.announce_once().await {
                Ok(out) => {
                    let now = (out.accepted, out.waiting_for.clone());
                    if out.changed || last_state.as_ref() != Some(&now) {
                        info!(
                            url = %self.url, accepted = out.accepted,
                            waiting_for = ?out.waiting_for,
                            "anunciado a la SDN",
                        );
                    }
                    last_state = Some(now);
                    out.accepted
                }
                Err(e) => {
                    if last_state.is_some() {
                        warn!(url = %self.url, error = %e, "perdí a la SDN; reintento");
                        last_state = None;
                    }
                    false
                }
            };
            let wait = if accepted {
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
