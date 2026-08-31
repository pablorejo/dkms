//! `QkcService` — handle Arc-shared entre todos los listeners y workers.

use arc_swap::ArcSwap;
use base64::Engine as _;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

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
    /// Frames de DATOS descartados en la puerta del peer_server porque la
    /// cola de intake estaba llena (contrapresión con la memoria acotada:
    /// esta capa no retransmite, y la alternativa al drop contado es el OOM).
    pub intake_dropped_full: AtomicU64,
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
    /// MAC de los frames de datos: integridad, autenticación de origen y
    /// frescura. `None` si el enlace no lo tiene activado o no hay `link_psk`.
    /// Aplica igual a QKD y a PQC — el OTP no da ninguna de las tres.
    pub frame_auth: Option<Arc<crate::frame_auth::LinkFrameAuth>>,
}

/// Bytes mínimos de la raíz simétrica de autenticación de enlace (`link_psk`).
/// 32 B = 256 bits, quantum-safe (Grover deja 128 efectivos).
pub const MIN_LINK_ROOT_BYTES: usize = 32;

/// `true` si una raíz de enlace decodificada tiene fuerza suficiente.
fn strong_link_root(len: usize) -> bool {
    len >= MIN_LINK_ROOT_BYTES
}

/// Cadena de certs DER (hoja primero) de un PEM. Mismo patrón que el ORR.
fn load_cert_chain_der(path: &std::path::Path) -> Result<Vec<Vec<u8>>, QkcError> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let pem =
        std::fs::read(path).map_err(|e| QkcError::BadRequest(format!("tls.cert_path: {e}")))?;
    let chain: Vec<Vec<u8>> = CertificateDer::pem_slice_iter(&pem)
        .map(|c| c.map(|c| c.as_ref().to_vec()))
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| QkcError::BadRequest(format!("tls.cert_path PEM: {e}")))?;
    if chain.is_empty() {
        return Err(QkcError::BadRequest(
            "tls.cert_path sin certificados".into(),
        ));
    }
    Ok(chain)
}

/// Identidad de certificado de nodo para el handshake firmado (modo `sign`
/// con certs). `None` sin `[tls]`; un `[tls]` presente pero ilegible cae a
/// `None` con aviso (queda el camino legacy sign_seed/peer_verify_key), en vez
/// de romper el arranque por un enlace.
fn build_sign_identity(
    cfg: &QkcConfig,
    neighbor_id: u32,
) -> Option<crate::pqc_handshake::SignIdentity> {
    let t = cfg.tls.as_ref()?;
    let load = || -> Result<crate::pqc_handshake::SignIdentity, QkcError> {
        let key_pem = std::fs::read(&t.key_path)
            .map_err(|e| QkcError::BadRequest(format!("tls.key_path: {e}")))?;
        let signer = common::crypto::pqc_sign::MlDsa65Signer::from_pkcs8_pem(&key_pem)
            .map_err(|e| QkcError::BadRequest(format!("tls.key_path ML-DSA: {e:?}")))?;
        let chain = load_cert_chain_der(&t.cert_path)?;
        let ca_pem = std::fs::read(&t.control_plane_ca)
            .map_err(|e| QkcError::BadRequest(format!("tls.control_plane_ca: {e}")))?;
        let roots = common::cert_identity::TrustRoots::from_pem(&ca_pem)
            .map_err(|e| QkcError::BadRequest(format!("tls.control_plane_ca PEM: {e}")))?;
        Ok(crate::pqc_handshake::SignIdentity {
            signer: Arc::new(signer),
            chain: Arc::new(chain),
            roots,
        })
    };
    match load() {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(neighbor = neighbor_id, error = %e,
                "qkc: [tls] presente pero la identidad de cert no carga; enlace en modo legacy");
            None
        }
    }
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
    /// Consumidores locales (el ORR). `ArcSwap` como `links` y la forwarding
    /// table: `deliver_local` corre por frame y con el mutex pagaba un lock +
    /// un clone del Vec cada vez; registrar/prunar es frío y va por RCU.
    pub local_out: Arc<arc_swap::ArcSwap<Vec<mpsc::Sender<wire::Frame>>>>,
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
            // Un vecino declarado solo por id espera a que la SDN diga dónde
            // está. Montarlo ahora daría un `PeerOut` apuntando a "", que
            // reintenta `invalid socket address` para siempre y deja el
            // keystore esperando un secreto que no va a llegar. El anuncio lo
            // lleva igual —solo viaja el id, nunca la dirección—, así que la
            // arista se crea en la SDN y su respuesta trae el `peer_addr`;
            // ahí lo levanta `apply_peers` con `add_link`.
            if link.neighbor_peer_addr.is_empty() {
                info!(
                    peer = link.neighbor_id,
                    "vecino declarado sin dirección: espero a que la SDN diga dónde está",
                );
                continue;
            }
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
            local_out: Arc::new(arc_swap::ArcSwap::from_pointee(Vec::new())),
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
            // Identidad de firma del nodo (cert de red, A1): sólo se carga en
            // enlaces PQC —los QKD se autentican contra su KME (A4)— y con ella
            // se deciden los defaults de auth por-salto (A3). Si `[tls]` carga,
            // el enlace va firmado + sellado por defecto; si no, modo legacy.
            // Un valor explícito en `pqc_auth`/`frame_auth` siempre manda.
            let sign_id = (link.link_type == LinkType::Pqc)
                .then(|| build_sign_identity(cfg, link.neighbor_id))
                .flatten();
            let has_node_identity = sign_id.is_some();
            let eff_pqc_auth = link.effective_pqc_auth(has_node_identity);
            let eff_frame_auth = link.effective_frame_auth(has_node_identity);
            // La fuente de claves depende del tipo de enlace; el resto del
            // KeyStore es idéntico (mismo hot path, mismo NOTIFY).
            let (kme, pqc, pqc_store): (
                Arc<dyn KeySource>,
                Option<Arc<PqcHandshake>>,
                Option<Arc<SecretStore>>,
            ) = match link.link_type {
                LinkType::Qkd => {
                    let url = link.quditto_url.clone().ok_or_else(|| {
                        QkcError::BadRequest(format!(
                            "QKD link to {} missing quditto_url",
                            link.neighbor_id
                        ))
                    })?;
                    // Identidad hacia ESTE KME (A6): la credencial de su PKI
                    // privada si el enlace la declara (kme_cert/kme_key/
                    // kme_ca — cada KME es una autoridad propia), y si no, la
                    // identidad de red del nodo, que es la simplificación de
                    // la prueba con quditto (acepta la net-ca).
                    let c = Arc::new(KmeClient::new(
                        url,
                        cfg.qkc_id.to_string(),
                        link.key_size_bits,
                        link.kme_client_tls(cfg.tls.as_ref()),
                    )?);
                    (c as Arc<dyn KeySource>, None, None)
                }
                LinkType::Pqc => {
                    // Re-keying por épocas (forward secrecy): el SecretStore +
                    // RekeyClock se comparten entre el handshake (escribe los
                    // secretos por época) y la PqcKeySource (deriva + cuenta).
                    let lookahead = link.effective_lookahead();
                    let is_initiator = cfg.qkc_id < link.neighbor_id;
                    let store = SecretStore::new(lookahead, link.pqc_rekey_keys);
                    let clock = RekeyClock::new(link.pqc_rekey_keys);
                    // PSK del enlace (base64) para autenticar el handshake
                    // (Fase 5). Un PSK mal formado se trata como ausente, con
                    // aviso: preferimos degradar a sin-auth que no montar el
                    // enlace por un typo en la config.
                    let decode_b64 = |b64: &str, what: &str| {
                        base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(|e| {
                                tracing::warn!(neighbor = link.neighbor_id, field = what, error = %e,
                                    "config base64 inválido; se ignora (enlace sin ese material)");
                            })
                            .ok()
                    };
                    // La raíz de la autenticación de enlace es simétrica y
                    // debe ser quantum-safe: 256 bits (Grover deja 128). Una
                    // PSK presente pero más corta es casi siempre un typo, y
                    // dejarla pasar da falsa sensación de seguridad. Se avisa
                    // alto y se ignora (como el base64 inválido); si el modo
                    // exige auth (`require`), el arranque falla luego por falta
                    // de PSK, que es justo lo que se quiere.
                    let psk = link
                        .link_psk
                        .as_deref()
                        .and_then(|b| decode_b64(b, "link_psk"))
                        .and_then(|k| {
                            if strong_link_root(k.len()) {
                                Some(k)
                            } else {
                                tracing::warn!(
                                    neighbor = link.neighbor_id,
                                    len = k.len(),
                                    min = MIN_LINK_ROOT_BYTES,
                                    "link_psk de menos de 32 B (no quantum-safe); se ignora"
                                );
                                None
                            }
                        });
                    // Modo `sign` (Fase 5 upgrade, ML-DSA): seed de firma de este
                    // nodo + clave pública de verificación del peer.
                    let sign_seed = cfg
                        .sign_secret_seed
                        .as_deref()
                        .and_then(|b| decode_b64(b, "sign_secret_seed"));
                    let peer_verify_key = link
                        .peer_verify_key
                        .as_deref()
                        .and_then(|b| decode_b64(b, "peer_verify_key"));
                    let hs = PqcHandshake::new(
                        link.pqc_suite.clone(),
                        cfg.qkc_id,
                        link.neighbor_id,
                        link.neighbor_peer_addr.clone(),
                        Arc::clone(peer_out) as Arc<dyn crate::pqc_handshake::HandshakeTransport>,
                        Arc::clone(&store),
                        Arc::clone(&clock),
                        lookahead,
                        link.pqc_rekey_secs,
                        link.key_size_bits,
                        psk,
                        eff_pqc_auth,
                        sign_seed,
                        peer_verify_key,
                        sign_id,
                    );
                    // El reloj solo lo alimenta el emisor del lado iniciador; el
                    // respondedor sigue la rotación vía store.highest().
                    let store_for_fa = Arc::clone(&store);
                    let src = Arc::new(PqcKeySource::new(
                        link.key_size_bits,
                        store,
                        lookahead,
                        is_initiator.then(|| Arc::clone(&clock)),
                    ));
                    (src as Arc<dyn KeySource>, Some(hs), Some(store_for_fa))
                }
            };
            // La clave del enlace autentica los NOTIFY (ver KeyStore::verify_notify).
            // Aplica a QKD y a PQC: es el plano de control del enlace, que el
            // material QKD no cubre.
            let notify_psk = link
                .link_psk
                .as_deref()
                .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok());
            // Los frames de datos usan la misma raíz que el handshake y los
            // NOTIFY. En `require` sin PSK se falla el arranque: seguir
            // adelante sería correr sin autenticar creyendo que sí, que es
            // exactamente lo que el flag existe para impedir.
            if eff_frame_auth.rejects_plaintext() && notify_psk.is_none() && pqc_store.is_none() {
                return Err(QkcError::BadRequest(format!(
                    "enlace {}: frame_auth = require exige link_psk o un enlace PQC (secreto de enlace)",
                    link.neighbor_id
                )));
            }
            // Raíz del MAC de frames, por orden de preferencia:
            //   1. `link_psk` explícita (raíz fija, QKD o PQC con psk).
            //   2. el secreto del enlace PQC (per-epoch, sin PSK que repartir;
            //      la autenticación la hereda del handshake firmado con cert).
            // Si no hay ninguna, no hay MAC (comportamiento histórico).
            let frame_auth = if notify_psk.is_some() {
                crate::frame_auth::LinkFrameAuth::new(eff_frame_auth, link.neighbor_id, notify_psk)
            } else {
                pqc_store.map(|store| {
                    crate::frame_auth::LinkFrameAuth::per_epoch(
                        eff_frame_auth,
                        link.neighbor_id,
                        store,
                    )
                })
            }
            .map(Arc::new);
            if eff_frame_auth.signs() && frame_auth.is_none() {
                warn!(
                    peer = link.neighbor_id,
                    "qkc.frame_auth: modo prefer sin raíz (ni link_psk ni enlace PQC); frames sin MAC"
                );
            }
            let keys = KeyStore::new(
                Arc::clone(&kme),
                Arc::clone(peer_out),
                link.neighbor_id,
                link.neighbor_peer_addr.clone(),
                cfg.qkc_id,
                link.key_size_bits,
                frame_auth.clone(),
            );
            Ok(LinkRuntime {
                cfg: link.clone(),
                kme,
                keys,
                pqc,
                frame_auth,
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
        self.spawn_link_state_logger(Duration::from_secs(5));
    }

    /// Línea `qkc.links` periódica: los enlaces vivos y los que se declararon
    /// y no existen.
    ///
    /// `keystore.levels` sale por enlace vivo, así que un enlace que no llegó
    /// a montarse no aparece en ningún sitio — y una ausencia no se ve. Desde
    /// que un vecino se puede declarar sólo por id, ese caso es normal
    /// durante unos segundos (se espera a que la SDN diga la dirección) y un
    /// fallo si se queda: `waiting` no vacío de forma persistente significa
    /// que la SDN no reconoce ese vecino, casi siempre porque el vecino no se
    /// ha registrado o porque el id está mal escrito en el `node.yml`.
    fn spawn_link_state_logger(&self, every: Duration) {
        let links = Arc::clone(&self.links);
        let cfg = Arc::clone(&self.cfg);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let snap = links.load();
                // Ordenados: dos vueltas seguidas tienen que poder compararse
                // de un vistazo.
                let mut live: Vec<u32> = snap.keys().copied().collect();
                live.sort_unstable();
                let mut waiting: Vec<u32> = cfg
                    .links
                    .iter()
                    .filter(|l| !snap.contains_key(&l.neighbor_id))
                    .map(|l| l.neighbor_id)
                    .collect();
                waiting.sort_unstable();
                info!(
                    me = cfg.qkc_id,
                    live = ?live,
                    declared = cfg.links.len(),
                    waiting = ?waiting,
                    "qkc.links",
                );
                // Una línea por enlace que autentica frames. Es lo que permite
                // ver desde fuera si el MAC está realmente activo (`signed`/
                // `verified` subiendo) y si algo lo está rechazando. Un enlace
                // sano tiene bad_mac = replayed = plain_rej = 0; que `plain_ok`
                // no baje a 0 tras el arranque significa que el otro extremo no
                // está firmando, que es la avería típica de config asimétrica.
                for peer in &live {
                    let Some(link) = snap.get(peer) else { continue };
                    let Some(fa) = &link.frame_auth else { continue };
                    let s = &fa.stats;
                    info!(
                        me = cfg.qkc_id,
                        peer,
                        mode = ?fa.mode(),
                        session = fa.session(),
                        signed = s.signed.load(Ordering::Relaxed),
                        verified = s.verified.load(Ordering::Relaxed),
                        bad_mac = s.bad_mac.load(Ordering::Relaxed),
                        replayed = s.replayed.load(Ordering::Relaxed),
                        plain_ok = s.plaintext_accepted.load(Ordering::Relaxed),
                        plain_rej = s.plaintext_rejected.load(Ordering::Relaxed),
                        "qkc.frame_auth",
                    );
                }
            }
        });
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
        let senders = self.local_out.load();
        let n_targets = senders.len();
        let mut n_ok = 0usize;
        let mut n_full = 0usize;
        let mut n_closed = 0usize;
        for tx in senders.iter() {
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
        // Throttled: con la cola del ORR llena sostenida era un warn por
        // frame; los contadores de arriba llevan la cuenta exacta.
        if n_ok == 0 && n_targets > 0 {
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    qkc = self.cfg.qkc_id,
                    targets = n_targets,
                    full = n_full,
                    closed = n_closed,
                    total = n + 1,
                    "qkc.deliver_local.no_consumer"
                );
            }
        }
    }

    pub fn register_local(&self, tx: mpsc::Sender<wire::Frame>) {
        self.local_out.rcu(|cur| {
            let mut next = (**cur).clone();
            next.push(tx.clone());
            next
        });
    }

    pub fn prune_local(&self) {
        self.local_out.rcu(|cur| {
            cur.iter()
                .filter(|tx| !tx.is_closed())
                .cloned()
                .collect::<Vec<_>>()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_root_must_be_256_bits() {
        assert!(!strong_link_root(0));
        assert!(!strong_link_root(16));
        assert!(!strong_link_root(31));
        assert!(strong_link_root(32));
        assert!(strong_link_root(64));
    }

    fn cfg_with(links: Vec<LinkConfig>) -> QkcConfig {
        QkcConfig {
            qkc_id: 1,
            peer_listen: "127.0.0.1:0".into(),
            local_listen: "127.0.0.1:0".into(),
            admin_http: "127.0.0.1:0".into(),
            sdn_url: None,
            advertise_ip: None,
            sdn_announce_secs: 30,
            tls: None,
            sign_secret_seed: None,
            links,
        }
    }

    fn pqc_link(neighbor: u32) -> LinkConfig {
        LinkConfig {
            neighbor_id: neighbor,
            neighbor_peer_addr: format!("127.0.0.1:{}", 20000 + neighbor),
            link_type: LinkType::Pqc,
            quditto_url: None,
            kme_cert: None,
            kme_key: None,
            kme_ca: None,
            pqc_suite: crate::config::default_pqc_suite(),
            key_size_bits: 256,
            pqc_rekey_keys: 1000,
            pqc_rekey_secs: 3600,
            pqc_rekey_lookahead: 2,
            r0: None,
            alpha: None,
            distance_km: None,
            capacity_keys_per_s: None,
            link_psk: None,
            pqc_auth: None,
            peer_verify_key: None,
            frame_auth: None,
        }
    }

    /// Un vecino declarado solo por id no se monta en el arranque: sin
    /// dirección, el `PeerOut` apuntaría a "" y reintentaría `invalid socket
    /// address` indefinidamente, con el keystore esperando un secreto que no
    /// llega. Se queda a la espera de que la SDN diga dónde está — y no entra
    /// en la tabla de enrutado, o se anunciaría como vecino directo un camino
    /// que no existe.
    #[tokio::test]
    async fn a_neighbour_without_an_address_waits_for_the_sdn() {
        let mut by_id = pqc_link(2);
        by_id.neighbor_peer_addr = String::new();
        let svc = QkcService::new(cfg_with(vec![by_id])).unwrap();

        assert!(svc.link_to(2).is_none(), "no se monta sin dirección");
        assert!(!svc.routing.is_direct_neighbor(2));

        // Cuando la SDN contesta con el `peer_addr`, `apply_peers` lo levanta
        // por la vía normal.
        assert!(svc.add_link(pqc_link(2)).unwrap());
        assert!(svc.link_to(2).is_some());
        assert!(svc.routing.is_direct_neighbor(2));
        assert_eq!(
            svc.neighbor_peer_addr(2).as_deref(),
            Some("127.0.0.1:20002"),
            "con la dirección que dijo la SDN",
        );
    }

    /// Los vecinos con dirección siguen montándose en el arranque: declararla
    /// es lo que permite levantar el enlace sin depender de la SDN.
    #[tokio::test]
    async fn a_neighbour_with_an_address_is_still_built_at_boot() {
        let svc = QkcService::new(cfg_with(vec![pqc_link(2)])).unwrap();
        assert!(svc.link_to(2).is_some());
        assert!(svc.routing.is_direct_neighbor(2));
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
