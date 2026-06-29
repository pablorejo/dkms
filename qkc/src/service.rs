//! `QkcService` — handle Arc-shared entre todos los listeners y workers.

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
use tracing::warn;

use crate::{
    config::{LinkConfig, LinkType, QkcConfig},
    error::QkcError,
    keystore::{self, KeyStore},
    kme::{KeySource, KmeClient},
    pqc_handshake::PqcHandshake,
    pqc_source::PqcKeySource,
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
    pub links: Arc<HashMap<u32, LinkRuntime>>,
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
                    let (hs, secret_rx) = PqcHandshake::new(
                        link.pqc_suite.clone(),
                        cfg.qkc_id,
                        link.neighbor_id,
                        link.neighbor_peer_addr.clone(),
                        Arc::clone(&peer_out),
                    );
                    let src = Arc::new(PqcKeySource::new(link.key_size_bits, secret_rx));
                    (src as Arc<dyn KeySource>, Some(hs))
                }
            };
            let keys = KeyStore::new(
                Arc::clone(&kme),
                Arc::clone(&peer_out),
                link.neighbor_id,
                link.neighbor_peer_addr.clone(),
                cfg.qkc_id,
                link.key_size_bits,
            );
            links.insert(
                link.neighbor_id,
                LinkRuntime {
                    cfg: link.clone(),
                    kme,
                    keys,
                    pqc,
                },
            );
            direct.insert(link.neighbor_id);
            if link.link_type == LinkType::Qkd {
                direct_qkd.insert(link.neighbor_id);
            }
        }
        let routing = ForwardingTable::new_with_grades(direct, direct_qkd);
        Ok(Self {
            cfg: Arc::new(cfg),
            routing: Arc::new(routing),
            links: Arc::new(links),
            peer_out,
            local_out: Arc::new(Mutex::new(Vec::new())),
            stats: Arc::new(ServiceStats::default()),
        })
    }

    /// Bootstrap de background tasks: arranca los workers de refill de
    /// cada KeyStore + un logger periódico de niveles.
    pub fn bootstrap_keystores(&self) {
        let mut for_logger = Vec::with_capacity(self.links.len());
        for (peer_id, link) in self.links.iter() {
            link.keys.spawn_workers();
            // Enlaces PQC: arranca el handshake ML-KEM (no-op en el lado
            // respondedor; el iniciador es el de qkc_id menor). Los workers
            // del KeyStore ya esperan el secreto vía el watch, así que no
            // hay carrera con el orden de arranque.
            if let Some(pqc) = &link.pqc {
                pqc.spawn_initiator();
            }
            for_logger.push((*peer_id, Arc::clone(&link.keys)));
        }
        keystore::spawn_level_logger(for_logger, Duration::from_secs(5));
    }

    pub fn qkc_id(&self) -> u32 {
        self.cfg.qkc_id
    }

    pub fn link_to(&self, neighbor_id: u32) -> Option<&LinkRuntime> {
        self.links.get(&neighbor_id)
    }

    pub fn neighbor_peer_addr(&self, neighbor_id: u32) -> Option<String> {
        self.links
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
