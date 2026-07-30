//! Anuncio del ORR a la SDN (`POST {sdn_http_url}/register/orr`).
//!
//! Va por HTTP y no por gRPC como el resto del plano de control del ORR (ver
//! [`crate::sdn_client`]) porque el registro es parte de la superficie de alta
//! del HTTP admin de la SDN, junto a `POST /sae` y `POST /demand`.
//!
//! El ORR se ancla a un QKC: la SDN rechaza un ORR cuyo `qkc_id` no conoce
//! todavía. Eso no es un error, es orden de arranque, así que esto es un bucle
//! con backoff igual que el del QKC. El reanuncio hace además de heartbeat.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, warn};

use crate::config::OrrConfig;

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
    qkc_id: String,
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
    /// `None` si no hay `sdn_http_url` (el auto-registro es opcional) o si no
    /// se puede deducir una IP alcanzable — en ese caso con un error logueado,
    /// pero sin impedir que el ORR arranque.
    pub fn from_config(cfg: &OrrConfig) -> Option<Self> {
        let base = cfg
            .sdn_http_url
            .as_deref()?
            .trim_end_matches('/')
            .to_string();

        let port: u16 = cfg
            .grpc_addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or(0);
        let bind_ip = cfg.grpc_addr.rsplit_once(':').map(|(h, _)| h).unwrap_or("");
        let ip = match cfg.advertise_ip.as_deref() {
            Some(ip) => ip.to_string(),
            None if !bind_ip.is_empty() && bind_ip != "0.0.0.0" && bind_ip != "[::]" => {
                bind_ip.to_string()
            }
            None => {
                error!(
                    grpc_addr = %cfg.grpc_addr,
                    "sdn_http_url configurado pero no sé con qué IP anunciarme: grpc_addr bindea \
                     a todas las interfaces. Pon `advertise_ip`. Sigo sin anunciarme.",
                );
                return None;
            }
        };

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .ok()?;

        Some(Self {
            http,
            url: format!("{base}/register/orr"),
            body: json!(Announce {
                id: cfg.orr_id.clone(),
                host: Host { id: 0, ip, port },
                qkc_id: cfg.qkc_id.to_string(),
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
