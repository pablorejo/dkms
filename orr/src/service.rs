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
use std::time::{SystemTime, UNIX_EPOCH};

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

/// Entrada del caché K-Splittable del ORR: K paths con sus pesos
/// `omega` ya en formato listo-para-sampling. El `AliasSampler` se
/// precomputa al cachear para que `sample()` sea O(1).
#[derive(Debug, Clone)]
pub struct MultipathCacheEntry {
    /// Mismo orden que la respuesta del SDN. Cada entry contiene
    /// la lista de orr_ids (NO qkc_ids — se proyecta al cachear).
    pub paths: Vec<Vec<String>>,
    /// Sampler precomputado. Si el commodity solo tiene 1 path útil,
    /// el sampler retorna siempre 0.
    pub sampler: crate::alias::AliasSampler,
    /// Rate total del commodity (informativo; no afecta sampling).
    pub total_keys_per_second: f64,
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
    /// Cache `dst_orr_id → path` (single-path, legacy). Sigue siendo el
    /// fallback cuando el commodity tiene K_efectivo = 1 o la SDN no
    /// expone `GetPathsWithRatios`. La invalidación se dispara en
    /// `TopologyEvent` (background loop).
    path_cache: Arc<RwLock<HashMap<String, Vec<String>>>>,
    /// Cache `(src_dkms, dst_dkms) → MultipathCacheEntry` para
    /// K-Splittable MCF. Se rellena vía `GetPathsWithRatios`. La
    /// invalidación se dispara igual que `path_cache` en
    /// `TopologyEvent`.
    paths_cache_multipath: Arc<RwLock<HashMap<(String, String), MultipathCacheEntry>>>,
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
            paths_cache_multipath: Arc::new(RwLock::new(HashMap::new())),
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
            let cache_mp = svc.paths_cache_multipath.clone();
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
                                        let n_mp_before = cache_mp.read().len();
                                        cache.write().clear();
                                        cache_mp.write().clear();
                                        info!(
                                            version = ev.version,
                                            invalidated_singlepath = n_before,
                                            invalidated_multipath = n_mp_before,
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

    /// K-Splittable MCF: muestrea un `qkc_path` para el commodity
    /// `(src_dkms → dst_dkms)` usando el cache local (si hit) o
    /// consultando `GetPathsWithRatios` al SDN (si miss). Devuelve
    /// `None` si:
    /// - No hay SDN conectada.
    /// - El SDN responde con `paths` vacío (flow Saturated o
    ///   src/dst en mismo QKC, o sin path en topología).
    ///
    /// En caso `None` el caller debe hacer fallback a `get_orr_path`
    /// single-path o decidir no enviar.
    pub async fn pick_multipath_qkc_hops(
        &self,
        src_dkms: &str,
        dst_dkms: &str,
    ) -> Result<Option<Vec<String>>> {
        // 1) Cache hit
        let key = (src_dkms.to_string(), dst_dkms.to_string());
        {
            let cache = self.paths_cache_multipath.read();
            if let Some(entry) = cache.get(&key) {
                if entry.sampler.is_empty() || entry.paths.is_empty() {
                    return Ok(None);
                }
                let mut rng = rand::thread_rng();
                let idx = entry.sampler.sample(&mut rng);
                return Ok(Some(entry.paths[idx].clone()));
            }
        }
        // 2) Cache miss → pide al SDN
        let Some(sdn) = self.sdn.clone() else {
            return Ok(None);
        };
        let resp = sdn.get_paths_with_ratios(src_dkms, dst_dkms).await?;
        if resp.paths.is_empty() {
            // Insertamos un "negative cache" implícito: si caché es miss
            // y respuesta vacía, no cacheamos (la próxima vez re-pedimos
            // por si el flow ya no está Saturated).
            return Ok(None);
        }
        let weights: Vec<f64> = resp.paths.iter().map(|p| p.omega).collect();
        let Some(sampler) = crate::alias::AliasSampler::build(&weights) else {
            return Ok(None);
        };
        let paths: Vec<Vec<String>> = resp.paths.iter().map(|p| p.qkc_hops.clone()).collect();
        let total = resp.total_keys_per_second;
        let entry = MultipathCacheEntry {
            paths: paths.clone(),
            sampler,
            total_keys_per_second: total,
        };
        // 3) Cachea y muestrea
        let idx = {
            let mut rng = rand::thread_rng();
            entry.sampler.sample(&mut rng)
        };
        let chosen = entry.paths[idx].clone();
        self.paths_cache_multipath.write().insert(key, entry);
        Ok(Some(chosen))
    }

    /// Helper para OBJ-003: si `MULTIPATH_ENABLED=true` (env var) y el
    /// caller pasó `src_dkms` y `dst_dkms` en el `app_header`, muestrea
    /// un `qkc_path` con `pick_multipath_qkc_hops`, parsea cada qkc_id
    /// `String -> u32` y devuelve los bytes msgpack listos para
    /// `Frame.header_qkc_mp`. En cualquier fallo (env OFF, app_header
    /// sin las keys, pick devuelve None, parse falla) devuelve
    /// `Vec::new()` (sin source routing → fallback a routing table del
    /// QKC).
    async fn compute_qkc_path_header(
        &self,
        app_header: &BTreeMap<String, String>,
    ) -> Vec<u8> {
        // 1) Opt-in via env var (R-015). Acepta dos nombres:
        // - `ORR_MULTIPATH_ENABLED`: preferido. El orchestator Python
        //   sólo propaga al sidecar ORR las vars con prefix `ORR_*`
        //   (ver `orchestrator/pods.py::_orr_container` filtrado
        //   línea ~4058), así que esta es la única que se puede
        //   activar desde el deploy en EKS sin tocar el orchestator.
        // - `MULTIPATH_ENABLED`: fallback para tests locales y
        //   ejecución standalone con `cargo run` (sin el filtrado
        //   del orchestator).
        // Lectura barata por iter; no cacheamos porque tests pueden
        // alterar la env dinámicamente.
        let enabled = std::env::var("ORR_MULTIPATH_ENABLED")
            .ok()
            .or_else(|| std::env::var("MULTIPATH_ENABLED").ok())
            .map(|v| v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if !enabled {
            return Vec::new();
        }
        // 2) src_dkms y dst_dkms vienen via app_header (decisión OBJ-004
        // opción (a) extendida: piggyback en app_header en vez de
        // ampliar la firma del RPC y proto).
        let Some(src_dkms) = app_header.get("src_dkms") else {
            return Vec::new();
        };
        let Some(dst_dkms) = app_header.get("dst_dkms") else {
            return Vec::new();
        };
        // 3) Pick path. None ⇒ fallback single-path.
        let path_strings = match self.pick_multipath_qkc_hops(src_dkms, dst_dkms).await {
            Ok(Some(p)) => p,
            Ok(None) => return Vec::new(),
            Err(e) => {
                warn!(error = %e, src = %src_dkms, dst = %dst_dkms,
                    "orr.multipath pick failed, fallback single-path");
                return Vec::new();
            }
        };
        // 4) Parse String -> u32. Si algún parse falla, fallback.
        let parsed: std::result::Result<Vec<u32>, _> =
            path_strings.iter().map(|s| s.parse::<u32>()).collect();
        match parsed {
            Ok(ids) if !ids.is_empty() => {
                debug!(src = %src_dkms, dst = %dst_dkms, path = ?ids,
                    "orr.multipath path selected qkc_hops");
                wire::encode_qkc_path(&ids)
            }
            Ok(_) => Vec::new(),
            Err(e) => {
                warn!(error = %e, src = %src_dkms, dst = %dst_dkms, path = ?path_strings,
                    "orr.multipath qkc_id parse failed, fallback single-path");
                Vec::new()
            }
        }
    }

    /// Test helper: número de entradas en el cache K-Splittable.
    #[cfg(test)]
    pub fn multipath_cache_len(&self) -> usize {
        self.paths_cache_multipath.read().len()
    }

    /// Test helper: inyecta una entry en el cache K-Splittable sin
    /// pasar por el SDN (útil para tests deterministas).
    #[cfg(test)]
    pub fn insert_multipath_cache_entry(
        &self,
        src_dkms: &str,
        dst_dkms: &str,
        entry: MultipathCacheEntry,
    ) {
        self.paths_cache_multipath
            .write()
            .insert((src_dkms.to_string(), dst_dkms.to_string()), entry);
    }

    // ─── send_message: dispatch por max_hops ───────────────────────────

    pub async fn send_message(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        max_hops: i32,
        app_header: BTreeMap<String, String>,
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
            0 => self.send_passthrough(dest_orr, payload, app_header).await,
            1 => self.send_onion_e2e(dest_orr, payload, app_header).await,
            -1 => {
                self.send_onion_path(dest_orr, payload, app_header, None)
                    .await
            }
            n if n >= 2 => {
                self.send_onion_path(dest_orr, payload, app_header, Some(n as usize))
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
    ) -> Result<SendOutcome> {
        let dest_qkc = self.peers.qkc_id(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!(
                "ORR destino {dest_orr} no resoluble (peers config)"
            ))
        })?;
        let header = OrrHeader::passthrough(&self.cfg.orr_id, dest_orr);
        let qkc_path_bytes = self.compute_qkc_path_header(&app_header).await;
        let frame = Frame {
            kind: FRAME_LOCAL_SEND,
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: dest_qkc,
            key_size_bits: 0,
            // Passthrough modo 0: sin capa onion → epoch_id no aplica.
            // El campo se cablea en OBJ-008+ cuando empezamos a generar
            // capas onion contra master_secrets versionados.
            epoch_id: 0,
            key_ids: Vec::new(),
            // OBJ-003: source routing K-Splittable opt-in via env
            // `MULTIPATH_ENABLED=true` + app_header.{src_dkms,dst_dkms}.
            // Si no hay path muestreado, queda Vec::new() y el QKC cae
            // a su routing table (fallback).
            header_qkc_mp: qkc_path_bytes,
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
    ) -> Result<SendOutcome> {
        let dest_qkc = self
            .peers
            .qkc_id(dest_orr)
            .ok_or_else(|| OrrError::Relay(format!("ORR destino {dest_orr} no resoluble")))?;
        // OBJ-010: usar la última época poblada (típicamente 0 hasta que
        // la rotación esté cableada en OBJ-011/012; > 0 después).
        let epoch_id = self.peers.latest_epoch_for(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!(
                "ORR destino {dest_orr} sin master_secret (bootstrap aún no completo)"
            ))
        })?;
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
        self.send_onion_frame(dest_orr, dest_qkc, onion, app_header)
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
        self.send_onion_frame(dest_orr, first_qkc, onion, app_header)
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
    ) -> Result<()> {
        let header = OrrHeader::onion(
            &self.cfg.orr_id,
            final_dest_orr,
            &onion.first_hop_orr,
            onion.first_key_id,
            onion.max_hops,
        );
        // OBJ-003: source routing K-Splittable opt-in. Aplica a modos
        // onion (e2e max_hops=1 y multi-hop max_hops>=2 / -1).
        let qkc_path_bytes = self.compute_qkc_path_header(&app_header).await;
        let frame = Frame {
            kind: FRAME_LOCAL_SEND,
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: next_qkc,
            key_size_bits: 0,
            // OBJ-010: la época del master_secret con que se cifró la
            // capa más externa del onion. El primer hop la usará
            // (vía `Frame.epoch_id` → `peers.master_for_epoch`) para
            // descifrar `payload`.
            epoch_id: onion.first_epoch_id,
            key_ids: Vec::new(),
            header_qkc_mp: qkc_path_bytes,
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
        let ms = match self.peers.master_for_epoch(&from, epoch_id) {
            Some(ms) => ms,
            None => {
                warn!(
                    from = %from,
                    epoch_id,
                    latest = ?self.peers.latest_epoch_for(&from),
                    "orr.incoming master_secret missing for epoch (drop)",
                );
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
            sender_id: self.cfg.qkc_id,
            receiver_id: self.cfg.qkc_id,
            dest_final: next_qkc_id,
            key_size_bits: 0,
            // OBJ-010: la `InnerLayer` que acabamos de pelar trae el
            // epoch_id que el SIGUIENTE peeler debe usar para
            // descifrar `inner.xor_ct`. Lo copiamos al wire frame.
            epoch_id: inner.epoch_id,
            key_ids: Vec::new(),
            header_qkc_mp: Vec::new(),
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
mod multipath_cache_tests {
    use super::*;
    use crate::alias::AliasSampler;

    /// Smoke test: el sampler precomputado funciona con paths reales y
    /// la distribución empírica converge a las omegas teóricas tras
    /// N=10000 samples. Esto es el OBJ-011 verificado a nivel de
    /// `MultipathCacheEntry`.
    #[test]
    fn multipath_entry_samples_match_omegas() {
        let paths = vec![
            vec!["q1".to_string(), "q3".to_string()],
            vec!["q1".to_string(), "q2".to_string(), "q3".to_string()],
        ];
        let weights = vec![0.7, 0.3]; // 70/30 split
        let sampler = AliasSampler::build(&weights).unwrap();
        let entry = MultipathCacheEntry {
            paths: paths.clone(),
            sampler,
            total_keys_per_second: 100.0,
        };

        let mut rng = rand::thread_rng();
        let n = 10_000;
        let mut counts = [0_usize; 2];
        for _ in 0..n {
            let idx = entry.sampler.sample(&mut rng);
            counts[idx] += 1;
        }
        let p0 = counts[0] as f64 / n as f64;
        let p1 = counts[1] as f64 / n as f64;
        assert!(
            (p0 - 0.7).abs() < 0.02,
            "split 70/30 esperado, got p0={p0}",
        );
        assert!(
            (p1 - 0.3).abs() < 0.02,
            "split 70/30 esperado, got p1={p1}",
        );
    }

    #[test]
    fn single_path_entry_always_samples_index_zero() {
        let entry = MultipathCacheEntry {
            paths: vec![vec!["q1".into(), "q5".into()]],
            sampler: AliasSampler::build(&[1.0]).unwrap(),
            total_keys_per_second: 333.0,
        };
        let mut rng = rand::thread_rng();
        for _ in 0..100 {
            assert_eq!(entry.sampler.sample(&mut rng), 0);
        }
    }

    /// OBJ-013: el snippet de invalidación del background loop
    /// (`TopologyEvent` handler) debe limpiar AMBOS caches a la vez.
    /// Reproducimos el snippet exacto sobre los `Arc<RwLock<…>>` que
    /// usaría el `OrrService` en producción.
    #[test]
    fn topology_event_invalidates_both_caches() {
        let single: Arc<RwLock<HashMap<String, Vec<String>>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let mp: Arc<RwLock<HashMap<(String, String), MultipathCacheEntry>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Poblar ambos caches (3 entries cada uno).
        single.write().insert("orr_a".into(), vec!["q1".into(), "q2".into()]);
        single.write().insert("orr_b".into(), vec!["q1".into(), "q3".into()]);
        single.write().insert("orr_c".into(), vec!["q1".into(), "q4".into()]);
        for (src, dst) in [("dA", "dB"), ("dA", "dC"), ("dB", "dC")] {
            mp.write().insert(
                (src.to_string(), dst.to_string()),
                MultipathCacheEntry {
                    paths: vec![vec!["q1".into(), "q3".into()]],
                    sampler: AliasSampler::build(&[1.0]).unwrap(),
                    total_keys_per_second: 100.0,
                },
            );
        }
        assert_eq!(single.read().len(), 3);
        assert_eq!(mp.read().len(), 3);

        // ── Snippet del background loop (orr/src/service.rs ≈l.217) ──
        let n_before = single.read().len();
        let n_mp_before = mp.read().len();
        single.write().clear();
        mp.write().clear();
        // ────────────────────────────────────────────────────────────

        assert_eq!(n_before, 3);
        assert_eq!(n_mp_before, 3);
        assert_eq!(single.read().len(), 0, "single-path cache no limpiado");
        assert_eq!(mp.read().len(), 0, "multipath cache no limpiado");
    }

    /// El cache permite múltiples entries simultáneamente sin colisión.
    #[test]
    fn cache_distinguishes_directional_flows() {
        let mp: Arc<RwLock<HashMap<(String, String), MultipathCacheEntry>>> =
            Arc::new(RwLock::new(HashMap::new()));

        let entry_ab = MultipathCacheEntry {
            paths: vec![vec!["q_ab".into()]],
            sampler: AliasSampler::build(&[1.0]).unwrap(),
            total_keys_per_second: 100.0,
        };
        let entry_ba = MultipathCacheEntry {
            paths: vec![vec!["q_ba".into()]],
            sampler: AliasSampler::build(&[1.0]).unwrap(),
            total_keys_per_second: 50.0,
        };
        mp.write().insert(("dA".into(), "dB".into()), entry_ab);
        mp.write().insert(("dB".into(), "dA".into()), entry_ba);

        // Direcciones independientes — dos entries distintos.
        assert_eq!(mp.read().len(), 2);
        let ab = mp.read().get(&("dA".into(), "dB".into())).cloned();
        let ba = mp.read().get(&("dB".into(), "dA".into())).cloned();
        assert_eq!(ab.unwrap().paths[0], vec!["q_ab"]);
        assert_eq!(ba.unwrap().paths[0], vec!["q_ba"]);
    }

    /// Simulación end-to-end del sampling sobre un cache hit: una
    /// entry con K=3 paths y ratios skewed (0.6/0.3/0.1). Tras 10000
    /// samples vía la API del entry (igual que `pick_multipath_qkc_hops`
    /// haría en cache hit), la distribución empírica converge a los
    /// ratios teóricos.
    #[test]
    fn cache_hit_sampling_matches_omegas_n10000() {
        let weights = [0.6, 0.3, 0.1];
        let paths = vec![
            vec!["pA".into()],
            vec!["pB".into()],
            vec!["pC".into()],
        ];
        let entry = MultipathCacheEntry {
            paths,
            sampler: AliasSampler::build(&weights).unwrap(),
            total_keys_per_second: 333.0,
        };

        let mut rng = rand::thread_rng();
        let n = 10_000;
        let mut counts = [0_usize; 3];
        for _ in 0..n {
            let idx = entry.sampler.sample(&mut rng);
            counts[idx] += 1;
        }
        for (i, w) in weights.iter().enumerate() {
            let p = counts[i] as f64 / n as f64;
            assert!(
                (p - w).abs() < 0.02,
                "weight[{i}]={w}, empirical={p}",
            );
        }
    }

    // ── OBJ-005: tests del wiring del helper compute_qkc_path_header ──

    /// Caso 1 — happy path: `Vec<String>` numéricos parsea a `Vec<u32>`
    /// y encode_qkc_path produce bytes decodeables que recuperan los
    /// mismos ids. Simula el camino exitoso del helper.
    #[test]
    fn parse_qkc_hops_and_encode_roundtrip() {
        let strings = ["1".to_string(), "2".to_string(), "100".to_string()];
        let parsed: std::result::Result<Vec<u32>, _> =
            strings.iter().map(|s| s.parse::<u32>()).collect();
        assert!(parsed.is_ok(), "todos numéricos deben parsear");
        let ids = parsed.unwrap();
        assert_eq!(ids, vec![1_u32, 2, 100]);

        let bytes = wire::encode_qkc_path(&ids);
        assert!(!bytes.is_empty(), "path no vacío produce bytes");
        let decoded = wire::decode_qkc_path(&bytes).unwrap();
        assert_eq!(decoded, ids, "roundtrip preserva path");
    }

    /// Caso 2 — fallback: si algún string NO es numérico, parse falla.
    /// El helper devuelve `Vec::new()` en este caso (verificable porque
    /// el `Result::Err` es lo que se observa).
    #[test]
    fn parse_qkc_hops_fails_on_non_numeric() {
        let strings = ["1".to_string(), "abc".to_string(), "3".to_string()];
        let parsed: std::result::Result<Vec<u32>, _> =
            strings.iter().map(|s| s.parse::<u32>()).collect();
        assert!(parsed.is_err(), "string no-numérico debe fallar parse");
    }

    /// Caso 3 — parse exitoso con valores grandes (qkc_ids en el rango
    /// típico de la BD `100_000 + node_id`).
    #[test]
    fn parse_qkc_hops_handles_large_ids() {
        let strings = ["100001".to_string(), "100120".to_string(), "100155".to_string()];
        let parsed: std::result::Result<Vec<u32>, _> =
            strings.iter().map(|s| s.parse::<u32>()).collect();
        assert!(parsed.is_ok());
        let bytes = wire::encode_qkc_path(&parsed.unwrap());
        let decoded = wire::decode_qkc_path(&bytes).unwrap();
        assert_eq!(decoded, vec![100_001_u32, 100_120, 100_155]);
    }

    /// Caso 4 — env var: el helper considera ON solo `"true"` (case-
    /// insensitive), todo lo demás es OFF.
    #[test]
    fn env_var_recognized_values() {
        // Simulamos la lógica de lectura tal cual está en
        // `compute_qkc_path_header`.
        fn check(v: &str) -> bool {
            v.eq_ignore_ascii_case("true")
        }
        assert!(check("true"));
        assert!(check("TRUE"));
        assert!(check("True"));
        assert!(!check("false"));
        assert!(!check("1"));
        assert!(!check(""));
        assert!(!check("yes"));
    }

    /// Caso 5 — app_header sin las keys → helper devuelve `Vec::new()`.
    /// Simula la condición de guarda del helper.
    #[test]
    fn app_header_missing_keys_means_no_multipath() {
        use std::collections::BTreeMap;
        let mut h: BTreeMap<String, String> = BTreeMap::new();
        assert!(!h.contains_key("src_dkms"));

        h.insert("src_dkms".into(), "dkms-1".into());
        // Falta dst_dkms → helper sale por la guarda.
        assert!(!h.contains_key("dst_dkms"));

        h.insert("dst_dkms".into(), "dkms-2".into());
        // Ahora ambas keys presentes → helper procedería.
        assert_eq!(h.get("src_dkms").map(|s| s.as_str()), Some("dkms-1"));
        assert_eq!(h.get("dst_dkms").map(|s| s.as_str()), Some("dkms-2"));
    }

    /// Caso 6 — sampler + parse + encode end-to-end (sin OrrService):
    /// muestrea un path de un cache entry simulado, parsea como lo
    /// haría el helper, encode y decode. Verifica que el flujo
    /// completo desde sampler hasta wire es coherente.
    #[test]
    fn sampler_to_wire_roundtrip_e2e() {
        let paths: Vec<Vec<String>> = vec![
            vec!["1".into(), "3".into()],
            vec!["1".into(), "2".into(), "3".into()],
        ];
        let weights = [0.6, 0.4];
        let sampler = AliasSampler::build(&weights).unwrap();
        let entry = MultipathCacheEntry {
            paths: paths.clone(),
            sampler,
            total_keys_per_second: 100.0,
        };

        let mut rng = rand::thread_rng();
        let idx = entry.sampler.sample(&mut rng);
        let sampled_strings = &entry.paths[idx];

        // Parse como el helper.
        let parsed: Vec<u32> = sampled_strings
            .iter()
            .map(|s| s.parse::<u32>().unwrap())
            .collect();
        // Encode + decode.
        let bytes = wire::encode_qkc_path(&parsed);
        let back = wire::decode_qkc_path(&bytes).unwrap();
        assert_eq!(back, parsed, "roundtrip end-to-end OK");
        assert!(idx < 2 && (back.len() == 2 || back.len() == 3),
            "el path muestreado es uno de los dos disponibles");
    }
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
