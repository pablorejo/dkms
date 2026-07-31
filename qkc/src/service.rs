//! `QkcService` — handle Arc-shared entre todos los listeners y workers.

use arc_swap::ArcSwap;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{
    config::{LinkConfig, LinkType, QkcConfig},
    error::QkcError,
    keystore::{self, KeyStore},
    kme::{KeySource, KmeClient},
    pqc_handshake::PqcHandshake,
    pqc_source::{PqcKeySource, RekeyClock, SecretStore},
    routing::ForwardingTable,
    transport::peer_client::PeerOut,
};

/// Contadores agregados por proceso del servicio. Todos AtomicU64
/// para poder leerse sin lock desde admin/stats y desde el level
/// logger. Sirven para diagnosticar dónde se pierden frames.
#[derive(Default)]
pub struct ServiceStats {
    /// Llamadas a `deliver_local` (= frames con dest_final == my_id
    /// que ya pasaron decrypt y van al ORR co-localizado).
    pub deliver_calls: AtomicU64,
    /// Suma de `try_send` que respondieron OK. Como cada call hace
    /// broadcast a N tx, esto NO coincide con `deliver_calls`.
    pub deliver_sends_ok: AtomicU64,
    /// `try_send` que respondieron `Full` — frame DROPPED silenciosamente
    /// porque la cola mpsc del consumidor estaba llena.
    pub deliver_drops_full: AtomicU64,
    /// `try_send` que respondieron `Closed` — el rx del consumidor ya
    /// se dropeó (writer task abortado, conexión cerrada).
    pub deliver_drops_closed: AtomicU64,
    /// Tareas `handle_local_send` que entraron en el handler.
    pub local_send_starts: AtomicU64,
    /// Tareas `handle_local_send` que devolvieron Ok.
    pub local_send_oks: AtomicU64,
    /// Tareas `handle_local_send` que devolvieron Err.
    pub local_send_errs: AtomicU64,
    /// Tareas `handle_incoming` que entraron en el handler.
    pub incoming_starts: AtomicU64,
    /// Tareas `handle_incoming` que entregaron localmente.
    pub incoming_delivered: AtomicU64,
    /// Tareas `handle_incoming` que reenviaron al siguiente salto.
    pub incoming_forwarded: AtomicU64,
    /// Tareas `handle_incoming` que devolvieron Err.
    pub incoming_errs: AtomicU64,
}

/// Estado por enlace al vecino directo.
pub struct LinkRuntime {
    pub cfg: LinkConfig,
    /// Fuente de claves del enlace: quditto-QKD ([`KmeClient`]) o PQC
    /// ([`PqcKeySource`]). El KeyStore solo la usa vía el trait.
    pub kme: Arc<dyn KeySource>,
    /// Buffers ENC/DEC + workers que mantienen las claves en memoria
    /// para no pasar por HTTP en el hot path.
    pub keys: Arc<KeyStore>,
    /// Coordinador del handshake ML-KEM (solo enlaces PQC; `None` en QKD).
    /// Lo consulta `peer_server` al recibir frames de handshake y lo
    /// arranca `bootstrap_keystores`.
    pub pqc: Option<Arc<PqcHandshake>>,
}

#[derive(Clone)]
pub struct QkcService {
    pub cfg: Arc<QkcConfig>,
    pub routing: Arc<ForwardingTable>,
    /// Enlaces vivos. `ArcSwap` como la `routing` de al lado: la SDN puede
    /// darle uno nuevo en caliente cuando aparece un vecino, sin reiniciar.
    /// Se lee por frame, así que la lectura tiene que ser barata.
    pub links: Arc<ArcSwap<HashMap<u32, Arc<LinkRuntime>>>>,
    pub peer_out: Arc<PeerOut>,
    /// Senders hacia conexiones locales (ORR).
    pub local_out: Arc<Mutex<Vec<mpsc::Sender<wire::Frame>>>>,
    /// Contadores de diagnóstico. Ver [`ServiceStats`].
    pub stats: Arc<ServiceStats>,
}

impl QkcService {
    pub fn new(cfg: QkcConfig) -> Result<Self, QkcError> {
        let peer_out = Arc::new(PeerOut::new());
        let mut links = HashMap::with_capacity(cfg.links.len());
        let mut direct = HashSet::with_capacity(cfg.links.len());
        // Direct neighbours reachable over a QKD link — a QKD-grade frame may
        // short-circuit only through these (a direct PQC neighbour must route
        // via the QKD table, possibly around a multi-hop QKD path).
        let mut direct_qkd = HashSet::with_capacity(cfg.links.len());
        for link in &cfg.links {
            let rt = Self::build_link(&cfg, link, &peer_out)?;
            direct.insert(link.neighbor_id);
            if link.link_type == LinkType::Qkd {
                direct_qkd.insert(link.neighbor_id);
            }
            links.insert(link.neighbor_id, Arc::new(rt));
        }
        let routing = ForwardingTable::new_with_grades(direct, direct_qkd);
        Ok(Self {
            cfg: Arc::new(cfg),
            routing: Arc::new(routing),
            links: Arc::new(ArcSwap::from_pointee(links)),
            peer_out,
            local_out: Arc::new(Mutex::new(Vec::new())),
            stats: Arc::new(ServiceStats::default()),
        })
    }

    /// Construye el runtime de UN enlace: fuente de claves, keystore y, si es
    /// PQC, su handshake. Extraído del constructor para poder crear enlaces
    /// después del arranque, cuando la SDN anuncia un vecino nuevo.
    fn build_link(
        cfg: &QkcConfig,
        link: &LinkConfig,
        peer_out: &Arc<PeerOut>,
    ) -> Result<LinkRuntime, QkcError> {
        {
            // La fuente de claves depende del tipo de enlace; el resto del
            // KeyStore es idéntico (mismo hot path, mismo NOTIFY).
            let (kme, pqc): (Arc<dyn KeySource>, Option<Arc<PqcHandshake>>) = match link.link_type {
                LinkType::Qkd => {
                    let url = link.quditto_url.clone().ok_or_else(|| {
                        QkcError::BadRequest(format!(
                            "QKD link to {} missing quditto_url",
                            link.neighbor_id
                        ))
                    })?;
                    let c = Arc::new(KmeClient::new(
                        url,
                        cfg.qkc_id.to_string(),
                        link.key_size_bits,
                    )?);
                    (c as Arc<dyn KeySource>, None)
                }
                LinkType::Pqc => {
                    // Re-keying por épocas (forward secrecy): el SecretStore +
                    // RekeyClock se comparten entre el handshake (escribe los
                    // secretos por época) y la PqcKeySource (deriva + cuenta).
                    let lookahead = link.effective_lookahead();
                    let is_initiator = cfg.qkc_id < link.neighbor_id;
                    let store = SecretStore::new(lookahead, link.pqc_rekey_keys);
                    let clock = RekeyClock::new(link.pqc_rekey_keys);
                    let hs = PqcHandshake::new(
                        link.pqc_suite.clone(),
                        cfg.qkc_id,
                        link.neighbor_id,
                        link.neighbor_peer_addr.clone(),
                        Arc::clone(peer_out),
                        Arc::clone(&store),
                        Arc::clone(&clock),
                        lookahead,
                        link.pqc_rekey_secs,
                    );
                    // El reloj solo lo alimenta el emisor del lado iniciador; el
                    // respondedor sigue la rotación vía store.highest().
                    let src = Arc::new(PqcKeySource::new(
                        link.key_size_bits,
                        store,
                        lookahead,
                        is_initiator.then(|| Arc::clone(&clock)),
                    ));
                    (src as Arc<dyn KeySource>, Some(hs))
                }
            };
            let keys = KeyStore::new(
                Arc::clone(&kme),
                Arc::clone(peer_out),
                link.neighbor_id,
                link.neighbor_peer_addr.clone(),
                cfg.qkc_id,
                link.key_size_bits,
            );
            Ok(LinkRuntime {
                cfg: link.clone(),
                kme,
                keys,
                pqc,
            })
        }
    }

    /// Bootstrap de background tasks: arranca los workers de refill de
    /// cada KeyStore + un logger periódico de niveles.
    pub fn bootstrap_keystores(&self) {
        let snap = self.links.load();
        let mut for_logger = Vec::with_capacity(snap.len());
        for (peer_id, link) in snap.iter() {
            link.keys.spawn_workers();
            // Enlaces PQC: arranca la tarea de rotación/pre-carga de épocas
            // (no-op en el respondedor, que es reactivo a los INIT; el
            // iniciador es el de qkc_id menor). Los workers del KeyStore
            // esperan el secreto de cada época vía el SecretStore, así que no
            // hay carrera con el orden de arranque.
            if let Some(pqc) = &link.pqc {
                pqc.spawn_rotation();
            }
            for_logger.push((*peer_id, Arc::clone(&link.keys)));
        }
        keystore::spawn_level_logger(for_logger, Duration::from_secs(5));
    }

    pub fn qkc_id(&self) -> u32 {
        self.cfg.qkc_id
    }

    pub fn link_to(&self, neighbor_id: u32) -> Option<Arc<LinkRuntime>> {
        self.links.load().get(&neighbor_id).cloned()
    }

    /// Da de alta un enlace en caliente. No-op si ya existe.
    ///
    /// El handshake se coordina solo: el iniciador es el de `qkc_id` menor y
    /// `spawn_rotation` es no-op en el respondedor, así que un enlace creado
    /// aquí arranca igual que uno del `node.yml`, sin coordinación extra.
    pub fn add_link(&self, link_cfg: LinkConfig) -> Result<bool, QkcError> {
        let id = link_cfg.neighbor_id;
        if self.links.load().contains_key(&id) {
            return Ok(false);
        }
        let rt = Arc::new(Self::build_link(&self.cfg, &link_cfg, &self.peer_out)?);
        let mut next = (**self.links.load()).clone();
        next.insert(id, Arc::clone(&rt));
        self.links.store(Arc::new(next));
        // El enrutado tiene que enterarse o el enlace existe pero no se usa.
        self.routing
            .add_direct(id, link_cfg.link_type == LinkType::Qkd);
        rt.keys.spawn_workers();
        if let Some(pqc) = &rt.pqc {
            pqc.spawn_rotation();
        }
        // El logger periódico de niveles se arrancó con una foto de los
        // enlaces del boot, así que un enlace añadido aquí no saldría nunca en
        // `keystore.levels`. Se le da el suyo propio: sin esto el enlace
        // funciona (se ve en `/stats`) pero es invisible en los logs, que es
        // por donde se mira cuando algo va mal.
        keystore::spawn_level_logger(vec![(id, Arc::clone(&rt.keys))], Duration::from_secs(5));
        info!(peer = id, kind = ?link_cfg.link_type, "enlace añadido en caliente");
        Ok(true)
    }

    /// Retira un enlace. Devuelve `true` si estaba.
    ///
    /// Al soltar el `LinkRuntime` se sueltan su `KeyStore` y su
    /// `PqcHandshake`, y con ellos el `SecretStore`, cuyas épocas son
    /// `Zeroizing`: el material del enlace se borra de memoria. Los frames que
    /// estuvieran en vuelo hacia ese peer ya tienen su `Arc`, así que terminan
    /// sin romperse; simplemente no habrá más.
    pub fn remove_link(&self, neighbor_id: u32) -> bool {
        if !self.links.load().contains_key(&neighbor_id) {
            return false;
        }
        let mut next = (**self.links.load()).clone();
        next.remove(&neighbor_id);
        self.links.store(Arc::new(next));
        self.routing.remove_direct(neighbor_id);
        info!(peer = neighbor_id, "enlace retirado; su material se libera");
        true
    }

    pub fn neighbor_peer_addr(&self, neighbor_id: u32) -> Option<String> {
        self.links
            .load()
            .get(&neighbor_id)
            .map(|l| l.cfg.neighbor_peer_addr.clone())
    }

    pub fn deliver_local(&self, frame: wire::Frame) {
        use tokio::sync::mpsc::error::TrySendError;
        self.stats.deliver_calls.fetch_add(1, Ordering::Relaxed);
        let senders = self.local_out.lock().clone();
        let n_targets = senders.len();
        let mut n_ok = 0usize;
        let mut n_full = 0usize;
        let mut n_closed = 0usize;
        for tx in senders {
            match tx.try_send(frame.clone()) {
                Ok(()) => {
                    n_ok += 1;
                }
                Err(TrySendError::Full(_)) => {
                    n_full += 1;
                }
                Err(TrySendError::Closed(_)) => {
                    n_closed += 1;
                }
            }
        }
        self.stats
            .deliver_sends_ok
            .fetch_add(n_ok as u64, Ordering::Relaxed);
        self.stats
            .deliver_drops_full
            .fetch_add(n_full as u64, Ordering::Relaxed);
        self.stats
            .deliver_drops_closed
            .fetch_add(n_closed as u64, Ordering::Relaxed);
        // Si el frame no llegó a NINGÚN consumidor, lo gritamos: es un
        // drop silencioso real (no había listener vivo o todos llenos).
        if n_ok == 0 && n_targets > 0 {
            warn!(
                qkc = self.cfg.qkc_id,
                targets = n_targets,
                full = n_full,
                closed = n_closed,
                "qkc.deliver_local.no_consumer"
            );
        }
    }

    pub fn register_local(&self, tx: mpsc::Sender<wire::Frame>) {
        self.local_out.lock().push(tx);
    }

    pub fn prune_local(&self) {
        self.local_out.lock().retain(|tx| !tx.is_closed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(links: Vec<LinkConfig>) -> QkcConfig {
        QkcConfig {
            qkc_id: 1,
            peer_listen: "127.0.0.1:0".into(),
            local_listen: "127.0.0.1:0".into(),
            admin_http: "127.0.0.1:0".into(),
            sdn_url: None,
            advertise_ip: None,
            sdn_announce_secs: 30,
            links,
        }
    }

    fn pqc_link(neighbor: u32) -> LinkConfig {
        LinkConfig {
            neighbor_id: neighbor,
            neighbor_peer_addr: format!("127.0.0.1:{}", 20000 + neighbor),
            link_type: LinkType::Pqc,
            quditto_url: None,
            pqc_suite: crate::config::default_pqc_suite(),
            key_size_bits: 256,
            pqc_rekey_keys: 1000,
            pqc_rekey_secs: 3600,
            pqc_rekey_lookahead: 2,
            r0: None,
            alpha: None,
            distance_km: None,
        }
    }

    #[tokio::test]
    async fn a_link_can_be_added_after_boot() {
        // Un QKC que arranca sin vecinos: antes esto era el estado final para
        // siempre, porque `links` se construía una vez.
        let svc = QkcService::new(cfg_with(vec![])).unwrap();
        assert!(svc.link_to(2).is_none());
        assert!(!svc.routing.is_direct_neighbor(2));

        assert!(svc.add_link(pqc_link(2)).unwrap());
        assert!(svc.link_to(2).is_some());
        // El enrutado tiene que enterarse: si no, el enlace existe pero nadie
        // lo usa.
        assert!(svc.routing.is_direct_neighbor(2));
    }

    #[tokio::test]
    async fn adding_a_link_twice_is_a_no_op() {
        let svc = QkcService::new(cfg_with(vec![])).unwrap();
        assert!(svc.add_link(pqc_link(2)).unwrap());
        // El anunciador lo llama en cada latido; un alta repetida no puede
        // reemplazar el KeyStore ni relanzar el handshake.
        assert!(!svc.add_link(pqc_link(2)).unwrap());
        assert_eq!(svc.links.load().len(), 1);
    }

    #[tokio::test]
    async fn removing_a_link_drops_it_from_routing_too() {
        let svc = QkcService::new(cfg_with(vec![pqc_link(2)])).unwrap();
        assert!(svc.routing.is_direct_neighbor(2));

        assert!(svc.remove_link(2));
        assert!(svc.link_to(2).is_none());
        assert!(!svc.routing.is_direct_neighbor(2));
        // Idempotente: quitar lo que ya no está no es un error.
        assert!(!svc.remove_link(2));
    }
}
