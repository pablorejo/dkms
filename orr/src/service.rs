//! Servicio ORR: pega gRPC (DKMS side) con el wire TCP del QKC local
//! y orquesta los cuatro modos de routing (passthrough / PQC E2E /
//! cebolla SDN / cebolla truncada).
//!
//! Mantiene:
//!   * `cfg`        — configuración inmutable.
//!   * `identity`   — par ML-KEM persistente del ORR (`OrrIdentity`).
//!   * `peers`      — directorio `orr_id → (qkc_id, pubkey)`.
//!   * `circuits`   — tabla de circuitos (sólo modos PQC/onion, opcional).
//!   * `qkc_link`   — conexión TCP persistente al QKC co-localizado.
//!   * `deliveries` — broadcast `tokio::sync::broadcast` de los
//!     `DeliveredMessage` que se entregan localmente.
//!
//! El pump de entrada (frames `FRAME_LOCAL_DELIVER` del QKC) llama a
//! [`OrrService::handle_incoming`], que:
//!   * Si la cabecera no es de tipo `ORR`, descarta.
//!   * Si `pqc_layer == true`, pela una capa onion con la sk local y
//!     o reenvía (`InnerLayer::Forward`) o entrega
//!     (`InnerLayer::Deliver`).
//!   * En caso contrario (passthrough), publica el payload tal cual.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Ventana mínima entre dos passive re-bootstraps consecutivos contra
/// el MISMO peer. Tras un re-handshake exitoso los frames undecryptable
/// cesan en ms; ver otro >30 s después indica un fallo real (no spam
/// del mismo intento todavía no propagado). Configurable a futuro si
/// algún cluster lento necesita más.
// Optimización 2026-05-25: bajado de 30 s a 5 s. Con el fix de pubkey
// refresh (Nivel 1) el handshake se completa en <100 ms en condiciones
// normales; 30 s era demasiado conservador y acumulaba minutos de
// recovery cuando varios peers se reinician en cascada (smoke
// n10-bootstrap-fix-buf8k: 17 min para que 9 commodities saliesen del
// stuck). Además, el rate-limit ahora solo aplica si el intento previo
// FALLÓ (ver `should_attempt_rebootstrap`), no a primeros intentos.
const PASSIVE_REBOOTSTRAP_MIN_INTERVAL: Duration = Duration::from_secs(5);

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use common::metrics::Metrics;
use common::proto::common::v1::NodeId;
use common::proto::orr::v1::DeliveredMessage;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use wire::{Frame, FRAME_LOCAL_SEND};

use crate::{
    bootstrap,
    config::OrrConfig,
    dkms_header,
    error::{OrrError, Result},
    header::{OrrHeader, HEADER_TYPE},
    identity::OrrIdentity,
    onion::{self, OnionWire, PathHopSecret, Peeled},
    peers::PeerRegistry,
    qkc_link::QkcLink,
    relay::CircuitTable,
    sdn_client::SdnClient,
};
use parking_lot::RwLock;

/// Resultado de un envío. Se exporta porque `grpc_server` lo traduce a
/// `SendMessageResponse`.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub status: &'static str,
    pub final_dest_orr: String,
    pub next_hop_qkc: u32,
    pub remaining_hops: i32,
    pub pqc_layer: bool,
}

#[derive(Clone)]
pub struct OrrService {
    pub cfg: Arc<OrrConfig>,
    pub identity: Arc<OrrIdentity>,
    pub circuits: Arc<CircuitTable>,
    pub peers: Arc<PeerRegistry>,
    pub metrics: Metrics,
    pub qkc_link: QkcLink,
    deliveries_tx: broadcast::Sender<DeliveredMessage>,
    /// Cliente gRPC contra la SDN. Sólo se usa en modos onion
    /// (`max_hops != 0,1`) cuando el caller no manda `orr_path` en el
    /// `app_header`. `None` ⇒ la SDN no estaba disponible al boot;
    /// el ORR seguirá funcionando si el caller provee `orr_path`.
    sdn: Option<Arc<SdnClient>>,
    /// Cache `dst_orr_id → path` single-path. La invalidación se
    /// dispara en `TopologyEvent` (background loop).
    path_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
}

impl OrrService {
    pub async fn new(mut cfg: OrrConfig, metrics: Metrics) -> Result<Self> {
        // Normalización: la crate `config` lowercase de forma silenciosa
        // todas las claves de HashMap al leer del TOML. Para que el lookup
        // `orr_id → qkc_id` no se descuadre, normalizamos también el
        // `orr_id` propio y las claves de `peer_pubkeys` a lowercase.
        // El usuario puede escribir `ORR_11` o `orr_11` en el TOML y
        // ambos funcionan idénticamente. Si el usuario informa de
        // colisión al normalizar (p.ej. dos peers que difieren solo en
        // case), abortamos con error explícito.
        cfg.orr_id = cfg.orr_id.to_lowercase();
        cfg.peers = lowercase_keys(cfg.peers, "peers")?;
        cfg.peer_pubkeys = lowercase_keys(cfg.peer_pubkeys, "peer_pubkeys")?;
        cfg.peer_grpc_addrs = lowercase_keys(cfg.peer_grpc_addrs, "peer_grpc_addrs")?;

        // Identidad ML-KEM (long-term, regenerada cada vez que arranca
        // el proceso — TODO: persistir en disco si queremos pubkeys
        // estables entre reinicios).
        let identity = Arc::new(OrrIdentity::generate(
            cfg.orr_id.clone(),
            &cfg.default_pqc_suite,
        )?);
        info!(
            orr_id = %cfg.orr_id,
            suite = %identity.suite,
            pubkey_len = identity.public_key.len(),
            "orr.identity generated",
        );

        // Decodificar pubkeys de peers desde base64.
        let mut peer_pubkeys: HashMap<String, Vec<u8>> = HashMap::new();
        for (orr_id, b64) in &cfg.peer_pubkeys {
            let bytes = BASE64.decode(b64).map_err(|e| {
                OrrError::Relay(format!("peer {orr_id} pubkey base64 inválido: {e}"))
            })?;
            peer_pubkeys.insert(orr_id.clone(), bytes);
        }

        let cfg = Arc::new(cfg);
        let peers = Arc::new(PeerRegistry::with_pubkeys(
            cfg.peers.clone(),
            peer_pubkeys,
            cfg.orr_id.clone(),
            cfg.qkc_id,
        ));

        let (deliveries_tx, _) = broadcast::channel(cfg.deliver_queue_capacity.max(1));
        let (frames_tx, mut frames_rx) = mpsc::channel::<Frame>(cfg.deliver_queue_capacity.max(1));
        let qkc_link = QkcLink::spawn(cfg.qkc_local_addr.clone(), frames_tx);

        // Conecta con la SDN (best-effort). Si no responde, seguimos
        // sin ella — los modos 0/1 no la necesitan y los onion la
        // suplen con `orr_path` en el header si lo trae el caller.
        let sdn = match SdnClient::connect_opt(&cfg.sdn_url).await {
            Ok(Some(c)) => {
                info!(endpoint = %cfg.sdn_url, "orr→sdn client connected");
                Some(Arc::new(c))
            }
            Ok(None) => {
                debug!("orr→sdn: sin endpoint configurado");
                None
            }
            Err(e) => {
                warn!(endpoint = %cfg.sdn_url, error = %e, "orr→sdn connect failed");
                None
            }
        };

        let svc = OrrService {
            cfg,
            identity,
            circuits: Arc::new(CircuitTable::new()),
            peers,
            metrics,
            qkc_link,
            deliveries_tx,
            sdn,
            path_cache: Arc::new(RwLock::new(HashMap::new())),
        };

        // Pump entrante: cada frame `FRAME_LOCAL_DELIVER` del QKC pasa
        // por `handle_incoming`, que decide pelar capa onion vs.
        // entregar directo.
        let pump = svc.clone();
        tokio::spawn(async move {
            while let Some(frame) = frames_rx.recv().await {
                if let Err(e) = pump.handle_incoming(frame).await {
                    warn!(error = %e, "orr.incoming handle_failed");
                }
            }
            warn!("orr.incoming pump stopped");
        });

        // Bootstrap pubkey + master_secret por peer. Cada task: pide
        // GetPublicKey con backoff, luego hace kem.encap(peer.pk) y
        // EstablishSecret RPC para que ambos lados queden con el
        // master_secret en sus tablas. Los modos onion (1, -1, ≥2) sólo
        // funcionan tras este bootstrap.
        bootstrap::spawn_all(
            svc.identity.clone(),
            svc.peers.clone(),
            svc.cfg.peer_grpc_addrs.clone(),
            svc.cfg.default_pqc_suite.clone(),
            svc.cfg.rotation_period_ms,
            svc.cfg.epoch_history_keep,
        );

        // Suscripción a topology events de la SDN. Cualquier evento
        // implica que el grafo cambió → vacía el path_cache para que
        // la próxima consulta re-pegue a SDN. Reconnect con backoff
        // exponencial si el stream cae.
        if let Some(sdn) = svc.sdn.clone() {
            let cache = svc.path_cache.clone();
            tokio::spawn(async move {
                let mut backoff_ms: u64 = 250;
                loop {
                    match sdn.stream_topology().await {
                        Ok(mut stream) => {
                            info!("orr.topology_subscriber connected");
                            backoff_ms = 250;
                            while let Some(item) = stream.message().await.transpose() {
                                match item {
                                    Ok(ev) => {
                                        let n_before = cache.read().len();
                                        cache.write().clear();
                                        info!(
                                            version = ev.version,
                                            invalidated = n_before,
                                            "orr.path_cache invalidated"
                                        );
                                    }
                                    Err(s) => {
                                        warn!(status = %s, "topology stream broken; reconnecting");
                                        break;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "topology stream subscribe failed; retrying");
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(5_000);
                }
            });
        }

        info!(
            orr_id = %svc.cfg.orr_id,
            qkc_id = svc.cfg.qkc_id,
            qkc_addr = %svc.cfg.qkc_local_addr,
            "orr service initialized",
        );
        Ok(svc)
    }

    /// Suscribirse al stream de mensajes entrantes locales.
    pub fn subscribe_deliveries(&self) -> broadcast::Receiver<DeliveredMessage> {
        self.deliveries_tx.subscribe()
    }

    /// Public key ML-KEM de este ORR (para que se la pidan los peers).
    pub fn public_key(&self) -> &[u8] {
        &self.identity.public_key
    }

    // ─── send_message: dispatch por max_hops ───────────────────────────

    pub async fn send_message(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        max_hops: i32,
        app_header: BTreeMap<String, String>,
        grade: u8,
    ) -> Result<SendOutcome> {
        // Normaliza el destino a lowercase para que el lookup en
        // `peers` (lowercased al boot — ver `lowercase_keys`) acepte
        // tanto `ORR_44` como `orr_44`.
        let dest_orr_lc = dest_orr.to_lowercase();
        let dest_orr = dest_orr_lc.as_str();

        // Entrega trivial a sí mismo (cualquier modo).
        if dest_orr == self.cfg.orr_id {
            self.broadcast_self(payload, app_header);
            return Ok(SendOutcome {
                status: "delivered_local",
                final_dest_orr: dest_orr.into(),
                next_hop_qkc: self.cfg.qkc_id,
                remaining_hops: 0,
                pqc_layer: false,
            });
        }

        match max_hops {
            0 => {
                self.send_passthrough(dest_orr, payload, app_header, grade)
                    .await
            }
            1 => {
                self.send_onion_e2e(dest_orr, payload, app_header, grade)
                    .await
            }
            -1 => {
                self.send_onion_path(dest_orr, payload, app_header, None, grade)
                    .await
            }
            n if n >= 2 => {
                self.send_onion_path(dest_orr, payload, app_header, Some(n as usize), grade)
                    .await
            }
            n => Err(OrrError::InvalidPath(format!("max_hops inválido: {n}"))),
        }
    }

    /// `max_hops = 0`: el ORR no añade capa onion. Empuja el payload
    /// (body_dkms = bytes crudos) al QKC con `dest_final = qkc(dest)`.
    /// Header ORR escribe metadatos propios; header DKMS propaga el
    /// `app_header` que viene del DKMS.
    async fn send_passthrough(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
        grade: u8,
    ) -> Result<SendOutcome> {
        let dest_qkc = self.peers.qkc_id(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!(
                "ORR destino {dest_orr} no resoluble (peers config)"
            ))
        })?;
        let header = OrrHeader::passthrough(&self.cfg.orr_id, dest_orr);
        let frame = Frame {
            kind: FRAME_LOCAL_SEND,
            grade,
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: dest_qkc,
            key_size_bits: 0,
            epoch_id: 0,
            key_ids: Vec::new(),
            header_orr_mp: header.encode()?,
            header_dkms_mp: dkms_header::encode(&app_header)?,
            payload,
        };
        self.qkc_link.send(frame).await?;
        debug!(dest = %dest_orr, dest_qkc, "orr.send passthrough");
        Ok(SendOutcome {
            status: "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc: dest_qkc,
            remaining_hops: 0,
            pqc_layer: false,
        })
    }

    /// `max_hops = 1`: una sola capa onion contra el ORR destino. El
    /// `orr_crypt{body}` es `body XOR K` donde K se deriva de un
    /// ML-KEM encap contra la pubkey del destino. El frame viaja por
    /// el QKC substrate; el ORR destino pela la capa antes de
    /// entregar.
    async fn send_onion_e2e(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
        grade: u8,
    ) -> Result<SendOutcome> {
        let dest_qkc = self
            .peers
            .qkc_id(dest_orr)
            .ok_or_else(|| OrrError::Relay(format!("ORR destino {dest_orr} no resoluble")))?;
        // OBJ-010: usar la última época poblada (típicamente 0 hasta que
        // la rotación esté cableada en OBJ-011/012; > 0 después).
        //
        // FIX Nivel 2 (2026-05-25): si no tenemos master_secret para
        // dest_orr, disparar trigger_passive_rebootstrap. Sin esto, el
        // sender se quedaba para siempre con el error "sin master_secret"
        // porque trigger_passive_rebootstrap solo se invocaba desde el
        // receiver (handle_onion_in), creando una asimetría: si el sender
        // perdía su master_secret y NO había tráfico entrante para
        // disparar la auto-cura desde el receiver, quedaba stuck (5/90
        // commodities en smoke 2026-05-25 n10-real16k). El próximo tick
        // del generator reintenta el send; mientras tanto, la task
        // spawnneada hace el re-handshake.
        let epoch_id = match self.peers.latest_epoch_for(dest_orr) {
            Some(e) => e,
            None => {
                self.trigger_passive_rebootstrap(dest_orr);
                return Err(OrrError::Relay(format!(
                    "ORR destino {dest_orr} sin master_secret (rebootstrap triggered, retry)"
                )));
            }
        };
        let ms = self
            .peers
            .master_for_epoch(dest_orr, epoch_id)
            .ok_or_else(|| {
                OrrError::Relay(format!(
                    "ORR destino {dest_orr} sin master_secret para epoch {epoch_id}"
                ))
            })?;
        let path = vec![PathHopSecret {
            orr_id: dest_orr.into(),
            master_secret: zeroize::Zeroizing::new(ms),
            epoch_id,
        }];
        let onion = onion::build_onion(&path, payload)?;
        self.send_onion_frame(dest_orr, dest_qkc, onion, app_header, grade)
            .await?;
        debug!(dest = %dest_orr, dest_qkc, "orr.send pqc_e2e");
        Ok(SendOutcome {
            status: "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc: dest_qkc,
            remaining_hops: 0,
            pqc_layer: true,
        })
    }

    /// `max_hops = -1` o `>=2`: cebolla multi-capa. El path viene del
    /// hint `app_header["orr_path"]` (CSV de orr_ids) mientras la SDN
    /// no esté cableada en Rust. Cuando llegue el cliente SDN, esto
    /// será una llamada a `SdnControl::ComputePath`.
    ///
    /// `cap = Some(N)` selecciona **N hops aleatorios** del path
    /// preservando el orden y garantizando que el destino siempre esté
    /// incluido como último (modo `max_hops >= 2`).
    /// `cap = None` toma el path completo (`max_hops = -1`).
    async fn send_onion_path(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
        cap: Option<usize>,
        grade: u8,
    ) -> Result<SendOutcome> {
        // Resolución de path en orden de prioridad:
        //   1. `app_header["orr_path"]` si viene (override explícito).
        //   2. Cache local `(dst_orr → path)` poblada por consultas
        //      previas a la SDN.
        //   3. RPC `GetOrrPath` a la SDN; la respuesta se cachea.
        // Sólo se pega a la red la PRIMERA vez por destino — las
        // siguientes salen de memoria.
        //
        // `cached_path` se materializa primero a Option<Vec<String>>
        // para liberar el `RwLockReadGuard` antes del posible .await
        // del fallback SDN (el guard de parking_lot no es Send).
        let cached_path: Option<Vec<String>> = self.path_cache.read().get(dest_orr).cloned();
        let hint_str = if let Some(h) = app_header.get("orr_path").cloned() {
            h
        } else if let Some(cached) = cached_path {
            cached.join(",")
        } else if let Some(sdn) = self.sdn.clone() {
            let path = sdn.get_orr_path(&self.cfg.orr_id, dest_orr).await?;
            if path.is_empty() {
                return Err(OrrError::InvalidPath(format!(
                    "sdn devolvió path vacío para {} → {}",
                    self.cfg.orr_id, dest_orr
                )));
            }
            self.path_cache
                .write()
                .insert(dest_orr.to_string(), path.clone());
            debug!(dest = %dest_orr, hops = path.len(), "orr.path cached from sdn");
            path.join(",")
        } else {
            return Err(OrrError::InvalidPath(
                "max_hops != 0,1 requiere `orr_path` en app_header o una SDN alcanzable".into(),
            ));
        };
        let mut full_path: Vec<String> = hint_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != &self.cfg.orr_id)
            .collect();
        if full_path.is_empty() {
            return Err(OrrError::InvalidPath(
                "orr_path vacío tras filtrar self".into(),
            ));
        }
        // Asegurar que el destino sea el último elemento.
        if full_path.last().map(|s| s.as_str()) != Some(dest_orr) {
            full_path.push(dest_orr.to_string());
        }
        // Aleatorización N hops (dest siempre incluido) si hay cap.
        if let Some(c) = cap {
            full_path = select_random_hops(full_path, c)?;
        }

        // Resolver cada hop (qkc_id + master_secret + epoch_id).
        let mut hops = Vec::with_capacity(full_path.len());
        for orr_id in &full_path {
            let _qkc_id = self.peers.qkc_id(orr_id).ok_or_else(|| {
                OrrError::Relay(format!("hop {orr_id} sin qkc_id en peers config"))
            })?;
            // OBJ-010: la época que se usa para cifrar la capa de este
            // hop es la última conocida. Pre-rotación cableada
            // (OBJ-011/012/013) este valor es 0 para todos los peers.
            let epoch_id = self.peers.latest_epoch_for(orr_id).ok_or_else(|| {
                OrrError::Relay(format!(
                    "hop {orr_id} sin master_secret (bootstrap incompleto)"
                ))
            })?;
            let ms = self
                .peers
                .master_for_epoch(orr_id, epoch_id)
                .ok_or_else(|| {
                    OrrError::Relay(format!(
                        "hop {orr_id} sin master_secret para epoch {epoch_id}"
                    ))
                })?;
            hops.push(PathHopSecret {
                orr_id: orr_id.clone(),
                master_secret: zeroize::Zeroizing::new(ms),
                epoch_id,
            });
        }
        let onion = onion::build_onion(&hops, payload)?;
        let first_qkc = self.peers.qkc_id(&onion.first_hop_orr).ok_or_else(|| {
            OrrError::Relay(format!(
                "first hop {} sin qkc_id en peers config",
                onion.first_hop_orr
            ))
        })?;
        let remaining = onion.max_hops;
        let first_orr = onion.first_hop_orr.clone();
        let n_hops = hops.len();
        self.send_onion_frame(dest_orr, first_qkc, onion, app_header, grade)
            .await?;
        debug!(
            dest = %dest_orr,
            first_hop = %first_orr,
            hops = n_hops,
            "orr.send onion_path",
        );
        Ok(SendOutcome {
            status: "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc: first_qkc,
            remaining_hops: remaining,
            pqc_layer: true,
        })
    }

    /// Mete un `OnionWire` dentro de un `FRAME_LOCAL_SEND` y lo encola
    /// al QKC local. El header lleva `next_orr_id` + `key_id` del
    /// `OnionWire` (lo que el peeler del primer hop usará para descifrar
    /// el `payload`).
    async fn send_onion_frame(
        &self,
        final_dest_orr: &str,
        next_qkc: u32,
        onion: OnionWire,
        app_header: BTreeMap<String, String>,
        grade: u8,
    ) -> Result<()> {
        let header = OrrHeader::onion(
            &self.cfg.orr_id,
            final_dest_orr,
            &onion.first_hop_orr,
            onion.first_key_id,
            onion.max_hops,
        );
        let frame = Frame {
            kind: FRAME_LOCAL_SEND,
            grade,
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: next_qkc,
            key_size_bits: 0,
            epoch_id: onion.first_epoch_id,
            key_ids: Vec::new(),
            header_orr_mp: header.encode()?,
            header_dkms_mp: dkms_header::encode(&app_header)?,
            payload: onion.payload,
        };
        self.qkc_link.send(frame).await?;
        Ok(())
    }

    // ─── incoming: pelar onion vs. entregar passthrough ────────────────

    async fn handle_incoming(&self, frame: Frame) -> Result<()> {
        let header = OrrHeader::decode(&frame.header_orr_mp)?;
        let dkms_map = dkms_header::decode(&frame.header_dkms_mp)?;

        // Filtra frames con type != ORR (otros protocolos sobre el
        // mismo QKC). No es un error, sólo no nos compete.
        if !header.kind.is_empty() && header.kind != HEADER_TYPE {
            debug!(kind = %header.kind, "orr.incoming ignore non-ORR");
            return Ok(());
        }

        match header.key_id {
            None => {
                // Modo 0 passthrough: el payload es body_dkms en claro
                // (el QKC ya lo descifró del enlace OTP).
                self.broadcast_delivery(&header, dkms_map, frame.payload);
                Ok(())
            }
            Some(kid) => {
                // Modo onion: pelar capa con master_secret_{from} de la
                // época indicada por el wire (`Frame.epoch_id`).
                self.handle_onion_in(&header, kid, frame.epoch_id, dkms_map, frame.payload)
                    .await
            }
        }
    }

    async fn handle_onion_in(
        &self,
        header: &OrrHeader,
        key_id: [u8; 16],
        epoch_id: u32,
        dkms_map: BTreeMap<String, String>,
        payload: Vec<u8>,
    ) -> Result<()> {
        let from = header.from.to_lowercase();
        // OBJ-010: lookup epoch-aware. Si el frame trae un `epoch_id`
        // que aún no rotamos con `from` (porque rotation.rs no
        // completó), `master_for_epoch` devuelve None → drop con warn,
        // jamás silencio. El emisor reintentará o caerá el frame.
        //
        // Passive re-bootstrap: si además es un peer conocido (= con
        // `peer_grpc_addrs` y `pubkey` ya cargados), disparamos un
        // re-handshake reactivo en background. Probablemente el peer
        // se reinició (rolling restart, OOM) y perdió todo su
        // `PeerRegistry`. Nuestro lado puede recuperar la sincronía
        // sin ayuda externa.
        let ms = match self.peers.master_for_epoch(&from, epoch_id) {
            Some(ms) => ms,
            None => {
                warn!(
                    from = %from,
                    epoch_id,
                    latest = ?self.peers.latest_epoch_for(&from),
                    "orr.incoming master_secret missing for epoch (drop)",
                );
                self.trigger_passive_rebootstrap(&from);
                return Ok(());
            }
        };
        let peeled = onion::peel(&ms, &key_id, &payload, header.max_hops)?;
        match peeled {
            Peeled::Deliver(body) => {
                debug!(from = %header.from, to = %header.to, "orr.onion deliver");
                self.broadcast_delivery(header, dkms_map, body);
                Ok(())
            }
            Peeled::Forward(inner) => {
                if inner.next_orr_id == self.cfg.orr_id {
                    return Err(OrrError::Relay("onion forward loop to self".into()));
                }
                let next_qkc = self.peers.qkc_id(&inner.next_orr_id).ok_or_else(|| {
                    OrrError::Relay(format!("forward hop {} sin qkc_id", inner.next_orr_id))
                })?;
                debug!(
                    next_orr = %inner.next_orr_id,
                    next_qkc,
                    "orr.onion forward",
                );
                self.forward_onion(header, dkms_map, inner, next_qkc).await
            }
        }
    }

    async fn forward_onion(
        &self,
        prev_header: &OrrHeader,
        dkms_map: BTreeMap<String, String>,
        inner: onion::InnerLayer,
        next_qkc_id: u32,
    ) -> Result<()> {
        let remaining_after = (prev_header.max_hops - 1).max(0);
        let new_header = OrrHeader::onion(
            &prev_header.from,
            &prev_header.to,
            &inner.next_orr_id,
            inner.key_id,
            remaining_after,
        );
        let out = Frame {
            kind: FRAME_LOCAL_SEND,
            grade: 0,
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: next_qkc_id,
            key_size_bits: 0,
            // OBJ-010: la `InnerLayer` que acabamos de pelar trae el
            // epoch_id que el SIGUIENTE peeler debe usar para
            // descifrar `inner.xor_ct`. Lo copiamos al wire frame.
            epoch_id: inner.epoch_id,
            key_ids: Vec::new(),
            header_orr_mp: new_header.encode()?,
            // El header_dkms se propaga byte-a-byte: no es nuestro.
            header_dkms_mp: dkms_header::encode(&dkms_map)?,
            payload: inner.xor_ct,
        };
        self.qkc_link.send(out).await
    }

    // ─── delivery helpers ──────────────────────────────────────────────

    fn broadcast_self(&self, payload: Vec<u8>, app_header: BTreeMap<String, String>) {
        let _ = self.deliveries_tx.send(DeliveredMessage {
            origin: Some(NodeId {
                value: self.cfg.orr_id.clone(),
            }),
            destination: Some(NodeId {
                value: self.cfg.orr_id.clone(),
            }),
            payload,
            app_header: app_header.into_iter().collect(),
            received_at_unix_ms: now_unix_ms(),
            pqc_decapsulated: false,
        });
    }

    fn broadcast_delivery(
        &self,
        header: &OrrHeader,
        dkms_map: BTreeMap<String, String>,
        payload: Vec<u8>,
    ) {
        let dest = if header.to.is_empty() {
            self.cfg.orr_id.clone()
        } else {
            header.to.clone()
        };
        let origin = if header.from.is_empty() {
            None
        } else {
            Some(header.from.clone())
        };
        let msg = DeliveredMessage {
            origin: origin.map(|v| NodeId { value: v }),
            destination: Some(NodeId { value: dest }),
            payload,
            app_header: dkms_map.into_iter().collect(),
            received_at_unix_ms: now_unix_ms(),
            pqc_decapsulated: header.key_id.is_some(),
        };
        if self.deliveries_tx.send(msg).is_err() {
            debug!("orr.deliver no_subscribers");
        }
    }

    // ─── passive re-bootstrap reactivo ────────────────────────────────
    //
    // Dispara un re-handshake ML-KEM con `peer_id` en background si:
    //   * el peer está en `peer_grpc_addrs` (= lo conocemos como peer
    //     de configuración, no es spam);
    //   * tenemos su `pubkey` cacheada (= el bootstrap inicial fetcheó
    //     la pubkey en algún momento);
    //   * pasó `PASSIVE_REBOOTSTRAP_MIN_INTERVAL` desde el último
    //     intento (rate-limit);
    //   * no hay otro intento in-flight para este peer (dedup).
    //
    // Si todo OK:
    //   1. Reserva el flag in-flight + marca el último intento ahora.
    //   2. Borra el material secreto stale del peer (`clear_peer_secrets`).
    //   3. `tokio::spawn` un task que llama a `bootstrap::attempt_establish`.
    //   4. Si la RPC retorna ok, guarda el `secret` como
    //      `bootstrap_secret` Y como `master_secret_epoch_0` (mismo
    //      workaround temporal que el bootstrap inicial — ver
    //      comentario en `bootstrap.rs:151-152`).
    //   5. Libera el flag in-flight.
    //
    // No es `async`: la decisión es síncrona; el handshake va en una
    // task spawnneada. Eso permite llamarlo desde `handle_onion_in`
    // sin tener que awaitear (y bloquear el pump de frames).
    fn trigger_passive_rebootstrap(&self, peer_id: &str) {
        // Filtro 1: peer en configuración. Sin esto un atacante podría
        // forzar floods de re-bootstrap mandándonos frames con `from`
        // arbitrarios. `peer_grpc_addrs` viene del TOML/configmap, es
        // confiable.
        let addr = match self.cfg.peer_grpc_addrs.get(peer_id) {
            Some(a) => a.clone(),
            None => {
                debug!(
                    peer = %peer_id,
                    "orr.passive_rebootstrap skip (peer not in peer_grpc_addrs)"
                );
                return;
            }
        };

        // Filtro 2: rate-limit. Tras un re-handshake exitoso los frames
        // undecryptable cesan en ms; ver otro >30 s después indica
        // fallo real, no spam.
        if !self
            .peers
            .should_attempt_rebootstrap(peer_id, PASSIVE_REBOOTSTRAP_MIN_INTERVAL)
        {
            debug!(
                peer = %peer_id,
                "orr.passive_rebootstrap skip (rate-limited)"
            );
            return;
        }

        // Filtro 3: dedup in-flight. Sólo un hilo entra a la vez por
        // peer. Si entran 100 frames undecryptable en 50 ms, sólo el
        // primero adquiere el flag.
        if !self.peers.try_mark_rebootstrap_inflight(peer_id) {
            debug!(
                peer = %peer_id,
                "orr.passive_rebootstrap skip (another inflight)"
            );
            return;
        }

        // Necesitamos la pubkey del peer. Si no la tenemos cargada
        // todavía, el bootstrap inicial no llegó tan lejos — un re-
        // bootstrap ahora no tiene material para encap. Liberamos el
        // flag y abortamos; el bootstrap inicial seguirá reintentando
        // su `fetch_pubkey`.
        let pk = match self.peers.public_key(peer_id) {
            Some(p) => p,
            None => {
                debug!(
                    peer = %peer_id,
                    "orr.passive_rebootstrap skip (pubkey not cached yet)"
                );
                self.peers.clear_rebootstrap_inflight(peer_id);
                return;
            }
        };

        // Optimización 2026-05-25: el marcado "este intento ocurrió" se
        // hace ahora dentro del task, condicionado al resultado:
        //   - Err → mark_rebootstrap_failure (futuro intento rate-limited)
        //   - Ok  → clear_rebootstrap_failure (siguientes intentos libres)
        // Antes se marcaba aquí incondicionalmente y eso aplicaba
        // rate-limit aunque el intento fuera a tener éxito.
        //
        // Borrado de claves stale ANTES del handshake. Si el RPC
        // fallara, mejor estar sin clave que con una mezcla
        // viejo/nuevo que produzca frames imposibles de descifrar.
        // El siguiente frame que llegue volverá a disparar.
        self.peers.clear_peer_secrets(peer_id);

        let peers = self.peers.clone();
        let identity = self.identity.clone();
        let suite = self.cfg.default_pqc_suite.clone();
        let local_orr_id = self.cfg.orr_id.clone();
        let peer_id_owned = peer_id.to_string();

        tokio::spawn(async move {
            info!(
                local = %local_orr_id,
                peer = %peer_id_owned,
                addr = %addr,
                "orr.passive_rebootstrap start"
            );
            // FIX Nivel 1 (2026-05-25): re-fetch pubkey ANTES del encap.
            //
            // Causa raíz: cada ORR genera un keypair ML-KEM fresco en
            // boot (identity.rs:32). Cuando un peer reinicia (rolling
            // update, OOM, kubectl set image), su pubkey CAMBIA pero
            // los demás ORRs siguen con la pubkey cacheada del boot
            // inicial (bootstrap.rs:94 — solo fetcha si is_none()).
            //
            // Sin este refresh: encap(pk_cached_OLD) produce ciphertext
            // que la sk_NEW del peer reiniciado no puede decapsular.
            // EstablishSecret retorna ok=false error="decap: ..."  y
            // este rebootstrap loop entra en bucle infinito de fallos
            // con rate-limit de 30s. Smoke 2026-05-25 n10-real16k dejó
            // 5/90 commodities con observed=0 por esta causa.
            //
            // Con el refresh: si el peer reinició, obtenemos su pubkey
            // actual y el encap subsiguiente produce un ciphertext que
            // la sk fresca sí puede decapsular.
            let pk = match bootstrap::try_fetch_pubkey(&addr).await {
                Ok((fresh_pk, _suite, reported)) => {
                    let reported_lc = reported.to_lowercase();
                    if !reported_lc.is_empty() && reported_lc != peer_id_owned {
                        warn!(
                            expected = %peer_id_owned,
                            reported = %reported,
                            addr = %addr,
                            "orr.passive_rebootstrap pubkey refresh reports different orr_id; aborting"
                        );
                        peers.clear_rebootstrap_inflight(&peer_id_owned);
                        return;
                    }
                    if fresh_pk != pk {
                        info!(
                            peer = %peer_id_owned,
                            old_len = pk.len(),
                            new_len = fresh_pk.len(),
                            "orr.passive_rebootstrap pubkey changed (peer restarted), updating cache",
                        );
                        peers.put_pubkey(peer_id_owned.clone(), fresh_pk.clone());
                    }
                    fresh_pk
                }
                Err(e) => {
                    warn!(
                        peer = %peer_id_owned,
                        addr = %addr,
                        error = %e,
                        "orr.passive_rebootstrap pubkey refresh failed (will retry on next undecryptable frame after rate-limit window)"
                    );
                    peers.clear_rebootstrap_inflight(&peer_id_owned);
                    return;
                }
            };
            let res = bootstrap::attempt_establish(
                &identity,
                &suite,
                &pk,
                &local_orr_id,
                &peer_id_owned,
                &addr,
            )
            .await;
            match res {
                Ok(secret) => {
                    // Mismo cableado que el bootstrap inicial: tanto
                    // bootstrap_secret como master_secret_epoch_0 (ver
                    // comentario en bootstrap.rs:151-152 sobre el
                    // workaround temporal de OBJ-011).
                    peers.set_bootstrap(peer_id_owned.clone(), secret);
                    peers.set_master_for_epoch(peer_id_owned.clone(), 0, secret);
                    // Limpiamos cualquier marca de fallo previo: el
                    // próximo trigger (si llega) no estará rate-limited.
                    peers.clear_rebootstrap_failure(&peer_id_owned);
                    info!(
                        peer = %peer_id_owned,
                        "orr.passive_rebootstrap ok"
                    );
                }
                Err(e) => {
                    // Marca este intento como fallido: el próximo trigger
                    // dentro de PASSIVE_REBOOTSTRAP_MIN_INTERVAL (5 s) se
                    // skip-eará. Tras la ventana se permite reintentar.
                    peers.mark_rebootstrap_failure(&peer_id_owned);
                    warn!(
                        peer = %peer_id_owned,
                        addr = %addr,
                        error = %e,
                        "orr.passive_rebootstrap failed (will retry on next undecryptable frame after rate-limit window)"
                    );
                }
            }
            peers.clear_rebootstrap_inflight(&peer_id_owned);
        });
    }
}

/// Devuelve un nuevo `HashMap` con todas las claves en lowercase. Si la
/// normalización produce colisión (p.ej. el TOML tenía `ORR_1` y `orr_1`),
/// devuelve `OrrError::Relay` para que el operador lo corrija. `field`
/// solo se usa en el mensaje de error.
fn lowercase_keys<V>(map: HashMap<String, V>, field: &'static str) -> Result<HashMap<String, V>> {
    let mut out: HashMap<String, V> = HashMap::with_capacity(map.len());
    for (k, v) in map {
        let lk = k.to_lowercase();
        if out.contains_key(&lk) {
            return Err(OrrError::Relay(format!(
                "{field}: clave duplicada tras lowercase (`{k}` colisiona con `{lk}`)"
            )));
        }
        out.insert(lk, v);
    }
    Ok(out)
}

/// Selecciona `n` hops del path preservando el orden, garantizando que
/// el último elemento (destino) esté siempre incluido. Si `n >= path.len()`
/// devuelve el path entero. Si `n == 0` falla.
///
/// Algoritmo:
///   1. Reserva slot para el destino (último).
///   2. De los `path.len()-1` intermedios, elige `n-1` al azar.
///   3. Reordena por posición original para preservar el orden del path.
fn select_random_hops(path: Vec<String>, n: usize) -> Result<Vec<String>> {
    use rand::seq::SliceRandom;

    if n == 0 {
        return Err(OrrError::InvalidPath("cap == 0".into()));
    }
    if n >= path.len() {
        return Ok(path);
    }
    let dest_idx = path.len() - 1;
    let dest = path[dest_idx].clone();

    // Indices de intermedios [0..dest_idx).
    let mut middle_indices: Vec<usize> = (0..dest_idx).collect();
    let mut rng = rand::thread_rng();
    middle_indices.shuffle(&mut rng);
    let pick = n.saturating_sub(1);
    let mut chosen_idx: Vec<usize> = middle_indices.into_iter().take(pick).collect();
    chosen_idx.sort_unstable();

    let mut out = Vec::with_capacity(n);
    for i in chosen_idx {
        out.push(path[i].clone());
    }
    out.push(dest);
    Ok(out)
}

#[cfg(test)]
mod lowercase_tests {
    use super::*;

    #[test]
    fn lowercase_keys_normalizes() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("ORR_A".into(), 1);
        m.insert("orr_b".into(), 2);
        let out = lowercase_keys(m, "test").unwrap();
        assert_eq!(out.get("orr_a"), Some(&1));
        assert_eq!(out.get("orr_b"), Some(&2));
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn lowercase_keys_detects_collision() {
        let mut m: HashMap<String, u32> = HashMap::new();
        m.insert("ORR_1".into(), 1);
        m.insert("orr_1".into(), 2);
        // Una de las dos colisionará al lowercased — error explícito.
        assert!(lowercase_keys(m, "peers").is_err());
    }

    #[test]
    fn lowercase_keys_empty() {
        let m: HashMap<String, u32> = HashMap::new();
        let out = lowercase_keys(m, "x").unwrap();
        assert!(out.is_empty());
    }
}

#[cfg(test)]
mod path_selection_tests {
    use super::*;

    #[test]
    fn n_zero_errors() {
        let path = vec!["A".into(), "B".into(), "C".into()];
        assert!(select_random_hops(path, 0).is_err());
    }

    #[test]
    fn n_one_returns_only_dest() {
        let path = vec!["A".into(), "B".into(), "C".into()];
        let out = select_random_hops(path, 1).unwrap();
        assert_eq!(out, vec!["C".to_string()]);
    }

    #[test]
    fn n_eq_path_returns_full_path() {
        let path = vec!["A".into(), "B".into(), "C".into()];
        let out = select_random_hops(path.clone(), 3).unwrap();
        assert_eq!(out, path);
    }

    #[test]
    fn n_greater_than_path_returns_full_path() {
        let path = vec!["A".into(), "B".into(), "C".into()];
        let out = select_random_hops(path.clone(), 10).unwrap();
        assert_eq!(out, path);
    }

    #[test]
    fn dest_always_included() {
        let path = vec!["A".into(), "B".into(), "C".into(), "D".into(), "E".into()];
        for _ in 0..100 {
            let out = select_random_hops(path.clone(), 3).unwrap();
            assert_eq!(out.len(), 3);
            assert_eq!(out.last().map(String::as_str), Some("E"));
        }
    }

    #[test]
    fn order_preserved() {
        let path = vec!["A".into(), "B".into(), "C".into(), "D".into(), "E".into()];
        // Posiciones originales: A=0, B=1, C=2, D=3, E=4. Cualquier
        // subset debe respetar ese orden.
        let pos = |s: &str| path.iter().position(|x| x == s).unwrap();
        for _ in 0..100 {
            let out = select_random_hops(path.clone(), 4).unwrap();
            for i in 0..out.len() - 1 {
                assert!(pos(&out[i]) < pos(&out[i + 1]));
            }
        }
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
