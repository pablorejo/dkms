//! Anuncio del ORR a la SDN (`POST {sdn_http_url}/register/orr`).
//!
//! Va por HTTP y no por gRPC como el resto del plano de control del ORR (ver
//! [`crate::sdn_client`]) porque el registro es parte de la superficie de alta
//! del HTTP admin de la SDN, junto a `POST /sae` y `POST /demand`.
//!
//! El ORR se ancla a un QKC: la SDN rechaza un ORR cuyo `qkc_id` no conoce
//! todavía. Eso no es un error, es orden de arranque, así que esto es un bucle
//! con backoff igual que el del QKC. El reanuncio hace además de heartbeat.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{error, info, warn};

use crate::{bootstrap, config::OrrConfig, identity::OrrIdentity, peers::PeerRegistry};

const BOOTSTRAP_RETRY: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Deserialize)]
struct Outcome {
    #[serde(default)]
    accepted: bool,
    #[serde(default)]
    changed: bool,
    #[serde(default)]
    waiting_for: Option<String>,
    /// Con quién debe hablar este ORR, según la SDN. Incluye pares que este
    /// nodo no tiene en su `node.yml`: es lo que permite que un ORR nuevo
    /// aparezca sin reconfigurar a los que ya estaban.
    #[serde(default)]
    orr_peers: Vec<OrrPeerWire>,
}

#[derive(Debug, Clone, Deserialize)]
struct OrrPeerWire {
    orr_id: String,
    qkc_id: String,
    grpc_url: String,
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
    /// Lo necesario para dar de alta un par en caliente: el registro donde
    /// anotarlo y lo que pide `bootstrap::spawn_one`.
    peers: Arc<PeerRegistry>,
    identity: Arc<OrrIdentity>,
    suite: String,
    rotation_period_ms: u64,
    epoch_history_keep: usize,
    /// Pares con bootstrap ya lanzado. La task persiste hasta lograrlo, así
    /// que lanzar dos para el mismo par serían dos bootstraps compitiendo.
    spawned: HashSet<String>,
    /// Pares del `node.yml`. **Nunca se retiran.** Mientras la SDN no conozca
    /// todavía a un par, su lista llega vacía; borrarlos entonces tiraría
    /// abajo su `master_secret` por un simple retraso.
    local_peers: HashSet<String>,
}

impl SdnAnnouncer {
    /// `None` si no hay `sdn_http_url` (el auto-registro es opcional) o si no
    /// se puede deducir una IP alcanzable — en ese caso con un error logueado,
    /// pero sin impedir que el ORR arranque.
    pub fn from_config(
        cfg: &OrrConfig,
        peers: Arc<PeerRegistry>,
        identity: Arc<OrrIdentity>,
    ) -> Option<Self> {
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

        let http = match common::http::announcer_client(
            &base,
            cfg.tls.as_ref().map(|t| t.as_client_tls()),
            Duration::from_secs(5),
        ) {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "no pude construir el cliente de anuncio (TLS?); no me anuncio");
                return None;
            }
        };

        Some(Self {
            http,
            url: format!("{base}/register/orr"),
            body: json!(Announce {
                id: cfg.orr_id.clone(),
                host: Host { id: 0, ip, port },
                qkc_id: cfg.qkc_id.to_string(),
            }),
            period: Duration::from_secs(cfg.sdn_announce_secs.max(1)),
            peers,
            identity,
            suite: cfg.default_pqc_suite.clone(),
            rotation_period_ms: cfg.rotation_period_ms,
            epoch_history_keep: cfg.epoch_history_keep,
            // Lo del node.yml ya lo arrancó `bootstrap::spawn_all`.
            spawned: cfg.peer_grpc_addrs.keys().cloned().collect(),
            local_peers: cfg.peer_grpc_addrs.keys().cloned().collect(),
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

    /// Aplica los pares que manda la SDN: alta de los nuevos (registro +
    /// bootstrap) y baja de los que ya no están (olvidando su material).
    fn apply_peers(&mut self, peers: &[OrrPeerWire]) {
        let wanted: HashSet<String> = peers.iter().map(|p| p.orr_id.clone()).collect();

        for p in peers {
            let Ok(qkc_id) = p.qkc_id.parse::<u32>() else {
                warn!(orr = %p.orr_id, qkc = %p.qkc_id, "qkc_id no numérico; ignoro el par");
                continue;
            };
            // `put` es idempotente, así que refrescarlo en cada latido no
            // cuesta nada y absorbe un cambio de QKC del par.
            self.peers.put(p.orr_id.clone(), qkc_id);
            // Refrescamos la URL en cada latido, no sólo al darlo de alta:
            // un peer redesplegado con otra IP debe seguir siendo
            // alcanzable para el re-bootstrap pasivo.
            self.peers
                .put_grpc_addr(p.orr_id.clone(), p.grpc_url.clone());
            if self.spawned.insert(p.orr_id.clone()) {
                info!(orr = %p.orr_id, url = %p.grpc_url, "par nuevo: arranco su bootstrap");
                bootstrap::spawn_one(
                    self.identity.clone(),
                    self.peers.clone(),
                    p.orr_id.clone(),
                    p.grpc_url.clone(),
                    self.suite.clone(),
                    self.rotation_period_ms,
                    self.epoch_history_keep,
                );
            }
        }

        let gone: Vec<String> = self
            .spawned
            .iter()
            .filter(|id| !wanted.contains(*id) && !self.local_peers.contains(*id))
            .cloned()
            .collect();
        for id in gone {
            info!(orr = %id, "par retirado por la SDN: olvido su material");
            self.peers.forget(&id);
            self.spawned.remove(&id);
        }
    }

    /// Bucle de anuncio. No termina nunca; va en su propia task.
    pub async fn run(mut self) {
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
                    if out.accepted {
                        self.apply_peers(&out.orr_peers);
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
