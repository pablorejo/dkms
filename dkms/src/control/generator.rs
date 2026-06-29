//! Generator: scheduler que rellena los buffers ENC compartidos con
//! cada peer DKMS al ritmo que dicte el SDN.
//!
//! Estructura por peer:
//!
//! ```text
//!   rate (HashMap<(peer, role), f64>)
//!     ↓ refill cada N ms
//!   token_bucket (HashMap<peer, BucketState>)
//!     ↓ consume tokens en cada tick
//!   key_bytes = random(key_size)
//!   key_id    = uuid4()
//!     ↓ insert
//!   ack_pending[peer][key_id] = (bytes, deadline)
//!     ↓ send via ORR
//!   orr.send_key(peer, bytes, header={msg_type=DKMS_BUFFER, key_id, ack_endpoint, …}, max_hops=1)
//! ```
//!
//! Cuando el peer recibe la clave (handle_orr_delivery), la guarda en su
//! `buffer_dec[source]` y manda FRAME_ACK por TCP a `ack_endpoint`. El
//! ACK socket de este DKMS lo recibe y llama a [`Generator::on_ack`],
//! que mueve la clave de `ack_pending` a `BufferPool.enc[peer]`.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use rand::RngCore;
use tracing::{debug, info, warn};
use uuid::Uuid;

use common::ids::KeyId;
use common::security::KeyGrade;

use crate::{
    config::{DkmsConfig, GeneratorCfg, PeerTransport},
    control::ack_pending::{AckPendingEntry, AckPendingStore},
    southbound::{
        orr::{
            HDR_ACK_ENDPOINT, HDR_KEY_ID, HDR_KEY_SIZE_BITS, HDR_MSG_TYPE, HDR_REQUEST_ID,
            HDR_SAE_ORIGIN, HDR_TIMESTAMP_MS, MSG_TYPE_DKMS_BUFFER,
        },
        OrrClient, SdnHttpClient,
    },
    state::{buffer::TransportKey, BufferPool},
};

/// Estado de un token bucket per (peer, role=ENC). El refill se calcula
/// en cada operación a partir del tiempo transcurrido y la rate cacheada.
#[derive(Debug, Clone, Copy)]
struct BucketState {
    /// Tokens disponibles (`f64` para soportar rates fraccionarias).
    tokens: f64,
    /// Último instante en que se refilló.
    last_refill: Instant,
}

impl BucketState {
    fn new(now: Instant) -> Self {
        Self {
            tokens: 0.0,
            last_refill: now,
        }
    }

    /// Refilla tokens según `rate × elapsed`, cap en `cap`. Devuelve la
    /// cantidad entera tomada respetando `max_take`.
    fn refill_and_take(&mut self, now: Instant, rate: f64, cap: f64, max_take: u32) -> u32 {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        self.tokens = (self.tokens + elapsed * rate).min(cap);
        self.last_refill = now;
        let to_take = (self.tokens as u32).min(max_take);
        self.tokens -= to_take as f64;
        if self.tokens < 0.0 {
            self.tokens = 0.0;
        }
        to_take
    }
}

#[derive(Clone)]
pub struct Generator {
    cfg: Arc<GeneratorCfg>,
    my_dkms_id: String,
    /// Mapa peer_dkms_id → orr_id, derivado de cfg.peers en el constructor.
    /// Solo se incluyen peers con `transport = "orr"`.
    peers_orr: Arc<HashMap<String, String>>,
    /// Rate cacheada por (peer, role). Solo usamos role=Enc para refill.
    rates_enc: Arc<Mutex<HashMap<String, f64>>>,
    /// `peer_dkms_id → qkd_available`, refrescado de `/rate` junto a las
    /// rates. Lo consulta `DkmsService::handle_enc_keys` para el admission
    /// de `strict_qkd` (rechaza si el SDN reporta que no hay camino QKD).
    qkd_avail: Arc<Mutex<HashMap<String, bool>>>,
    buckets: Arc<Mutex<HashMap<String, BucketState>>>,
    /// Contador acumulativo de claves que llegaron a `buffer_enc[peer]`
    /// vía ACK. Permite calcular la **rate efectiva de fill** comparando
    /// dos snapshots — clave para validar la rate asignada por el SDN.
    /// `peer_dkms_id → contador`. Se accede sin lock porque AtomicU64.
    emit_counters: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
    pool: Arc<BufferPool>,
    pub ack_pending: Arc<AckPendingStore>,
    orr: Arc<OrrClient>,
    sdn_http: Arc<SdnHttpClient>,
    /// EWMA tracker shared with [`crate::service::DkmsService`].
    /// The service updates it on every SAE request; the demand loop
    /// here reads it to build the `POST /demand` payload sent to
    /// the SDN.
    demand_tracker: crate::demand_tracker::SharedDemandTracker,
    /// Snapshot of `BufferCfg.capacity_per_peer` used as `B_k` in
    /// the demand report. Stays constant for the lifetime of the
    /// Generator (topology mutations don't resize buffers).
    buffer_capacity_per_peer: usize,
    ack_endpoint: Option<String>,
    default_max_hops: i32,
}

impl Generator {
    /// Construye el Generator. No lanza ningún task — para arrancar los
    /// loops llamar a [`Generator::spawn_background`].
    pub fn new(
        cfg: &DkmsConfig,
        orr: Arc<OrrClient>,
        sdn_http: Arc<SdnHttpClient>,
        pool: Arc<BufferPool>,
        ack_pending: Arc<AckPendingStore>,
        demand_tracker: crate::demand_tracker::SharedDemandTracker,
    ) -> Self {
        let mut peers_orr = HashMap::new();
        for (peer_id, pc) in &cfg.peers {
            if pc.transport == PeerTransport::Orr {
                if let Some(orr_id) = &pc.orr_id {
                    peers_orr.insert(peer_id.clone(), orr_id.clone());
                } else {
                    warn!(
                        peer = peer_id,
                        "generator: peer transport=orr sin orr_id configurado, se ignora"
                    );
                }
            }
        }
        // Prefer the explicit advertised endpoint (DNS-routable in K8s)
        // over the bind SocketAddr (which would stringify as 0.0.0.0:PORT).
        let ack_endpoint = cfg
            .generator
            .ack_advertised_endpoint
            .clone()
            .or_else(|| cfg.generator.ack_socket_addr.map(|a| a.to_string()));
        Self {
            cfg: Arc::new(cfg.generator.clone()),
            my_dkms_id: cfg.node_id.clone(),
            peers_orr: Arc::new(peers_orr),
            rates_enc: Arc::new(Mutex::new(HashMap::new())),
            qkd_avail: Arc::new(Mutex::new(HashMap::new())),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            emit_counters: Arc::new(Mutex::new(HashMap::new())),
            pool,
            ack_pending,
            orr,
            sdn_http,
            demand_tracker,
            buffer_capacity_per_peer: cfg.buffer.capacity_per_peer,
            ack_endpoint,
            default_max_hops: cfg.southbound.default_max_hops,
        }
    }

    /// Handle a la tabla de rates polleada del SDN. Útil para compartir
    /// con el `SaeBufferBuckets` que necesita la misma `link_capacity`
    /// para calcular `refill = capacity / N_active_SAEs`.
    pub fn rates_handle(&self) -> Arc<Mutex<HashMap<String, f64>>> {
        self.rates_enc.clone()
    }

    /// Conectividad QKD del peer según el último `/rate`: `Some(true|false)`
    /// si el SDN ya respondió, `None` en bootstrap (aún sin info). El
    /// admission de `strict_qkd` solo rechaza ante `Some(false)`.
    pub fn qkd_available(&self, peer: &str) -> Option<bool> {
        self.qkd_avail.lock().get(peer).copied()
    }

    /// Obtiene (o crea) el counter acumulativo para un peer.
    fn counter_for(&self, peer: &str) -> Arc<AtomicU64> {
        let mut g = self.emit_counters.lock();
        g.entry(peer.to_owned())
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone()
    }

    /// Lanza los 3 loops del Generator (refresh de rates, scheduler tick,
    /// reaper de ack_pending). Devuelve un `JoinSet` para que el caller
    /// pueda await o abort.
    pub fn spawn_background(self) -> Arc<Self> {
        let me = Arc::new(self);
        if !me.cfg.enabled {
            info!("generator: deshabilitado por config");
            return me;
        }
        if me.peers_orr.is_empty() {
            info!("generator: ningún peer con transport=orr — no se generan claves");
            return me;
        }
        let rate_loop = me.clone();
        tokio::spawn(async move {
            rate_loop.run_rate_refresh_loop().await;
        });
        let tick_loop = me.clone();
        tokio::spawn(async move {
            tick_loop.run_tick_loop().await;
        });
        let reaper_loop = me.clone();
        tokio::spawn(async move {
            reaper_loop.run_reaper_loop().await;
        });
        let log_loop = me.clone();
        tokio::spawn(async move {
            log_loop.run_state_log_loop().await;
        });
        let demand_loop = me.clone();
        tokio::spawn(async move {
            demand_loop.run_demand_loop().await;
        });
        info!(
            my_dkms = %me.my_dkms_id,
            n_peers = me.peers_orr.len(),
            tick_ms = me.cfg.tick_ms,
            ack_timeout_ms = me.cfg.ack_timeout_ms,
            demand_refresh_ms = me.cfg.demand_refresh_ms,
            "generator background started",
        );
        me
    }

    /// Cada 5s loguea el estado de buffers ENC, DEC, ack_pending y la
    /// **rate de emit observada** comparando contadores cumulativos
    /// entre snapshots — comparar con la rate asignada por el SDN para
    /// validar el plumbing.
    async fn run_state_log_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // saltar el primer tick inmediato
        tick.tick().await;

        let mut prev: HashMap<String, (u64, Instant)> = HashMap::new();
        loop {
            tick.tick().await;
            let now = Instant::now();
            let pool_snap = self.pool.snapshot();
            let ack_snap: HashMap<String, usize> =
                self.ack_pending.snapshot().into_iter().collect();
            let rates_snap = self.rates_enc.lock().clone();
            let counters_snap: HashMap<String, u64> = {
                let g = self.emit_counters.lock();
                g.iter()
                    .map(|(p, c)| (p.clone(), c.load(Ordering::Relaxed)))
                    .collect()
            };
            // Unión de peers que aparecen en pool_snap y en peers_orr.
            let mut all_peers: std::collections::BTreeSet<String> =
                pool_snap.iter().map(|(p, _, _)| p.clone()).collect();
            for p in self.peers_orr.keys() {
                all_peers.insert(p.clone());
            }
            for peer in all_peers {
                let (enc_len, dec_len) = pool_snap
                    .iter()
                    .find(|(p, _, _)| p == &peer)
                    .map(|(_, e, d)| (*e, *d))
                    .unwrap_or((0, 0));
                let ack_pending = ack_snap.get(&peer).copied().unwrap_or(0);
                let rate_sdn = rates_snap.get(&peer).copied().unwrap_or(0.0);
                let emit_total = counters_snap.get(&peer).copied().unwrap_or(0);
                let observed_rate = if let Some((prev_total, prev_t)) = prev.get(&peer) {
                    let dt = now.saturating_duration_since(*prev_t).as_secs_f64();
                    if dt > 0.0 {
                        (emit_total.saturating_sub(*prev_total) as f64) / dt
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                prev.insert(peer.clone(), (emit_total, now));
                info!(
                    peer = %peer,
                    enc = enc_len,
                    dec = dec_len,
                    ack_pending,
                    emit_total,
                    observed_keys_per_s = format!("{observed_rate:.1}"),
                    sdn_rate_keys_per_s = format!("{rate_sdn:.1}"),
                    "generator.state",
                );
            }
        }
    }

    /// Punto de entrada del ACK socket: el peer confirma que recibió la
    /// clave `key_id`. Movemos la entrada de `ack_pending[peer]` a
    /// `BufferPool.enc[peer]`. Devuelve `true` si se movió.
    pub fn on_ack(&self, peer_dkms_id: &str, key_id: &KeyId) -> bool {
        let Some(entry) = self.ack_pending.take(peer_dkms_id, key_id) else {
            debug!(peer = peer_dkms_id, key_id = %key_id, "generator.on_ack miss");
            return false;
        };
        let buf = self.pool.for_peer(peer_dkms_id);
        let grade = entry.grade;
        let key = TransportKey {
            id: key_id.clone(),
            bytes: entry.bytes,
        };
        if let Err(rejected) = buf.enc(grade).try_push(key) {
            // Buffer lleno: la clave se descarta (zeroize en Drop).
            warn!(
                peer = peer_dkms_id,
                key_id = %rejected.id,
                "generator.on_ack: buffer_enc full, key dropped",
            );
            return false;
        }
        self.counter_for(peer_dkms_id)
            .fetch_add(1, Ordering::Relaxed);
        debug!(peer = peer_dkms_id, key_id = %key_id, "generator.ack ok → buffer_enc");
        true
    }

    // ────────────────────── internal loops ──────────────────────────────

    /// Polling periódico de `GET /rate/{dkms_id}`. Actualiza el caché
    /// `rates_enc`. Reintentos con backoff si el SDN no responde.
    async fn run_rate_refresh_loop(self: Arc<Self>) {
        let period = Duration::from_millis(self.cfg.rate_refresh_ms);
        let mut backoff_ms = 500u64;
        loop {
            match self.sdn_http.get_rates(&self.my_dkms_id).await {
                Ok(resp) => {
                    let mut by_peer: HashMap<String, f64> = HashMap::new();
                    let mut qkd: HashMap<String, bool> = HashMap::new();
                    for (peer, rate) in resp.peers.iter() {
                        // Solo nos interesa la rate de ENC (generación).
                        // DEC la lleva el SDN simétricamente pero no la
                        // usamos para refill del bucket local.
                        by_peer.insert(peer.clone(), rate.enc);
                        qkd.insert(peer.clone(), rate.qkd_available);
                    }
                    let n = by_peer.len();
                    *self.rates_enc.lock() = by_peer;
                    *self.qkd_avail.lock() = qkd;
                    backoff_ms = 500;
                    debug!(
                        peers = n,
                        topology_version = resp.topology_version,
                        "generator.rates refreshed from SDN",
                    );
                }
                Err(e) => {
                    debug!(
                        error = %e,
                        backoff_ms,
                        "generator.rates refresh failed; will retry",
                    );
                    tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                    backoff_ms = (backoff_ms * 2).min(10_000);
                    continue;
                }
            }
            tokio::time::sleep(period).await;
        }
    }

    /// Scheduler tick: por cada peer, consume tokens según rate y emite
    /// claves vía ORR. Cada tick es independiente del anterior.
    async fn run_tick_loop(self: Arc<Self>) {
        let tick_period = Duration::from_millis(self.cfg.tick_ms);
        loop {
            let started = Instant::now();
            self.tick_once(started).await;
            let elapsed = started.elapsed();
            if elapsed < tick_period {
                tokio::time::sleep(tick_period - elapsed).await;
            }
        }
    }

    async fn tick_once(self: &Arc<Self>, now: Instant) {
        // SDN-assigned ENC rates (lock corto). Pueden venir vacías / todas a 0
        // hasta el primer solve MCMCF-λ del SDN, que a N grande puede tardar
        // minutos (el LP no escala). Caemos a un `floor` configurable para que
        // los buffers se llenen y el tráfico SAE fluya con independencia del
        // optimizador de rates. floor = 0 (default) preserva el comportamiento
        // legacy (solo se rellena con lo que asigne el SDN).
        let sdn_rates: HashMap<String, f64> = {
            let g = self.rates_enc.lock();
            g.clone()
        };
        let floor = self.cfg.default_fill_rate_keys_per_s;
        // Cap (techo) opcional de la rate efectiva: `min(max(sdn, floor), cap)`.
        // Con cap=0 no se aplica. Usado para saturación controlada cuando el SDN
        // está sano y asignaría una rate muy por encima de λ (ver config.rs).
        let fill_cap = self.cfg.max_fill_rate_keys_per_s;
        // Iteramos los peers ORR CONFIGURADOS (no solo aquellos para los que el
        // SDN ya reportó rate), así un peer se llena incluso antes de que el SDN
        // lo reporte por primera vez.
        let work: Vec<(String, f64)> = self
            .peers_orr
            .keys()
            .map(|p| {
                let mut r = sdn_rates.get(p).copied().unwrap_or(0.0).max(floor);
                if fill_cap > 0.0 {
                    r = r.min(fill_cap);
                }
                (p.clone(), r)
            })
            .collect();
        for (peer, rate) in work {
            if rate <= 0.0 {
                continue;
            }
            let cap = (self.cfg.bucket_cap_seconds * rate).max(2.0);
            let tokens = {
                let mut b = self.buckets.lock();
                let st = b
                    .entry(peer.clone())
                    .or_insert_with(|| BucketState::new(now));
                st.refill_and_take(now, rate, cap, self.cfg.max_tokens_per_peer_per_tick)
            };
            if tokens == 0 {
                continue;
            }
            // Si el buffer_enc destino está lleno, no quemamos tokens
            // generando: ya tenemos material reservado pero pendiente
            // de consumo. Esto evita acumular keys que se zeroizan al
            // expirar el ack_pending sin uso.
            let buf = self.pool.for_peer(&peer);
            if buf.enc_len() + self.ack_pending.pending_count(&peer)
                >= self.buffer_capacity_per_peer
            {
                continue;
            }
            // Concurrent fan-out de los `tokens` emits hacia este peer.
            // El loop anterior era `for _ in 0..tokens { emit_key().await }`
            // estrictamente secuencial, lo que pinaba el throughput per-peer
            // a `tick_period / per_emit_latency`. En cluster K8s, con tonic
            // gRPC al ORR sidecar a ~1-3 ms por send_key, el techo era
            // ~330 kps por peer — incompatible con lo que la SDN dictaba
            // a commodities en paths cortos (1.5 kkps). `join_all` lanza
            // los futures concurrentemente sobre el mismo task tokio:
            // tonic.Channel multiplexa HTTP/2 streams en paralelo sobre
            // una sola conexión TCP, así que N emits concurrentes ≈ N×
            // throughput vs el secuencial.
            //
            // Aggregamos los errores en una sola línea warn por tick por
            // peer (en vez de N warns separadas) para evitar log storms
            // durante el bootstrap ORR — verificado smoke 2026-05-25:
            // con tokens=400 y peer ORR sin master_secret aún, el código
            // anterior generaba 4000 warn/seg/pod = 752 MB de logs en
            // ~10 min, saturando disk I/O del pod.
            let futs = (0..tokens).map(|_| {
                let me = self.clone();
                let peer = peer.clone();
                async move { me.emit_key(&peer).await }
            });
            let results = futures::future::join_all(futs).await;
            let total = results.len();
            let mut ok = 0usize;
            let mut sample_err: Option<String> = None;
            for r in results {
                match r {
                    Ok(_) => ok += 1,
                    Err(e) => {
                        if sample_err.is_none() {
                            sample_err = Some(e.to_string());
                        }
                    }
                }
            }
            let failed = total - ok;
            if failed > 0 {
                warn!(
                    peer = %peer,
                    ok,
                    failed,
                    sample_error = sample_err.as_deref().unwrap_or("?"),
                    "generator.emit batch had failures",
                );
            }
        }
    }

    /// Emite UNA clave hacia el peer: genera bytes, registra en
    /// ack_pending, envía vía ORR.
    async fn emit_key(self: Arc<Self>, peer_dkms_id: &str) -> anyhow::Result<()> {
        let Some(dest_orr_id) = self.peers_orr.get(peer_dkms_id).cloned() else {
            return Ok(()); // no debería pasar (filtrado antes)
        };
        let mut bytes = vec![0u8; self.cfg.key_size_bytes];
        rand::thread_rng().fill_bytes(&mut bytes);
        let key_id_str = Uuid::new_v4().to_string();
        let key_id = KeyId::new(&key_id_str);
        let deadline = Instant::now() + Duration::from_millis(self.cfg.ack_timeout_ms);
        // Registramos ANTES de mandar — si el ACK llega antes del .await
        // del send (poco probable pero existe), el on_ack ya tiene la
        // entrada.
        // Grade of this transport key = the peer's QKD reachability per the
        // SDN /rate: QKD-reachable → QKD-grade (relayed strictly over QKD
        // links), else PQC-grade. The frame carries it (QKC routes per grade)
        // and `on_ack` files the key in the matching enc buffer.
        let grade = if self
            .qkd_avail
            .lock()
            .get(peer_dkms_id)
            .copied()
            .unwrap_or(true)
        {
            KeyGrade::Qkd
        } else {
            KeyGrade::Pqc
        };
        let entry_bytes = bytes.clone();
        self.ack_pending.insert(
            peer_dkms_id,
            key_id.clone(),
            AckPendingEntry::new(entry_bytes, deadline, grade),
        );

        let mut header: BTreeMap<String, String> = BTreeMap::new();
        header.insert(HDR_MSG_TYPE.into(), MSG_TYPE_DKMS_BUFFER.into());
        header.insert(HDR_KEY_ID.into(), key_id_str.clone());
        header.insert(HDR_SAE_ORIGIN.into(), self.my_dkms_id.clone());
        header.insert(
            HDR_KEY_SIZE_BITS.into(),
            (self.cfg.key_size_bytes * 8).to_string(),
        );
        header.insert(
            HDR_TIMESTAMP_MS.into(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
                .to_string(),
        );
        header.insert(HDR_REQUEST_ID.into(), format!("gen-{key_id_str}"));
        if let Some(ep) = &self.ack_endpoint {
            header.insert(HDR_ACK_ENDPOINT.into(), ep.clone());
        }
        if let Err(e) = self
            .orr
            .send_key(
                &dest_orr_id,
                bytes,
                header,
                self.default_max_hops,
                grade.wire_byte(),
            )
            .await
        {
            // Si el ORR falla, no esperamos ACK — retiramos del pending.
            let _ = self.ack_pending.take(peer_dkms_id, &key_id);
            return Err(anyhow::anyhow!(e.to_string()));
        }
        Ok(())
    }

    async fn run_reaper_loop(self: Arc<Self>) {
        let period = Duration::from_millis(self.cfg.ack_reaper_ms);
        loop {
            let removed = self.ack_pending.reap_expired(Instant::now());
            if removed > 0 {
                warn!(removed, "generator.ack_reaper: expired pending keys");
            }
            tokio::time::sleep(period).await;
        }
    }

    /// Build a `DemandReport` for the current set of ORR-routed peers
    /// and POST it to the SDN's `/demand` endpoint. One entry per
    /// peer, even when its EWMA is 0 — the SDN solver needs to see
    /// every commodity it might allocate rate to.
    ///
    /// Empty batches short-circuit inside `SdnHttpClient::post_demand`
    /// (no network I/O), so a DKMS without peers stays quiet.
    fn build_demand_report(&self, now_ms: i64) -> crate::southbound::DemandReport {
        use crate::southbound::{CommodityDemand, DemandReport};
        let cap = self.buffer_capacity_per_peer as f64;
        let entries: Vec<CommodityDemand> = self
            .peers_orr
            .keys()
            // Skip self: the DKMS config lists every DKMS (including
            // this one) under `peers.<id>` so the SaeBindingCache can
            // resolve every SAE. The MCMCF-λ commodity space excludes
            // self-loops; the SDN rejects them with `malformed entry`
            // otherwise.
            .filter(|peer| peer.as_str() != self.my_dkms_id.as_str())
            .map(|peer| {
                let level = self.pool.for_peer(peer).enc_len() as f64;
                let drain_rate = self.demand_tracker.rate(peer, now_ms);
                // Grade of this commodity = the peer's QKD reachability, so the
                // SDN builds a QKD-grade commodity for QKD-reachable peers and
                // a PQC-grade one otherwise (matches how the keys are pumped).
                let grade = if self.qkd_avail.lock().get(peer).copied().unwrap_or(true) {
                    KeyGrade::Qkd
                } else {
                    KeyGrade::Pqc
                };
                CommodityDemand {
                    src_dkms: self.my_dkms_id.clone(),
                    dst_dkms: peer.clone(),
                    level,
                    capacity: cap,
                    drain_rate,
                    timestamp_ms: now_ms,
                    grade,
                }
            })
            .collect();
        DemandReport {
            dkms_id: self.my_dkms_id.clone(),
            entries,
        }
    }

    /// Periodic reporter that drives the SDN's `DemandRegistry`.
    /// Every `demand_refresh_ms` (default 1 s) it snapshots
    /// `(level, capacity, δ_k)` per peer and POSTs the batch to the
    /// SDN. The MCMCF-λ solver reads from that registry on its next
    /// recompute.
    ///
    /// Why: the v13-validation campaign found 17-33% of POSTs were
    /// dropping at the transport layer during SAE ramps (mesh topos:
    /// 1445-7615 fails over a single run), leaving the SDN with stale
    /// δ_k and r_k that never tracked demand growth. Retry locally
    /// before accepting a lost tick so the EWMA at the SDN side keeps
    /// up.
    async fn run_demand_loop(self: Arc<Self>) {
        const MAX_ATTEMPTS: u32 = 3;
        // Backoff schedule (ms) chosen so the worst case (3 attempts
        // with two waits) stays well below demand_refresh_ms=1000.
        const BACKOFF_MS: [u64; 2] = [40, 120];
        let period = Duration::from_millis(self.cfg.demand_refresh_ms.max(50));
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now_ms = chrono::Utc::now().timestamp_millis();
            let report = self.build_demand_report(now_ms);
            if report.entries.is_empty() {
                continue;
            }
            let mut last_err: Option<anyhow::Error> = None;
            let mut applied_ok: Option<crate::southbound::DemandApplied> = None;
            for attempt in 1..=MAX_ATTEMPTS {
                match self.sdn_http.post_demand(&report).await {
                    Ok(applied) => {
                        applied_ok = Some(applied);
                        break;
                    }
                    Err(e) => {
                        last_err = Some(e);
                        if attempt < MAX_ATTEMPTS {
                            // Jitter ±25% to avoid 20 DKMSs synchronously
                            // retrying against a stressed SDN.
                            let base = BACKOFF_MS[(attempt - 1) as usize];
                            let jitter = (rand::thread_rng().next_u64() % (base / 2 + 1)) as i64
                                - (base / 4) as i64;
                            let wait = (base as i64 + jitter).max(1) as u64;
                            tokio::time::sleep(Duration::from_millis(wait)).await;
                        }
                    }
                }
            }
            match (applied_ok, last_err) {
                (Some(applied), _) => {
                    if !applied.errors.is_empty() {
                        warn!(
                            n_errors = applied.errors.len(),
                            registry_len = applied.registry_len,
                            "generator.demand SDN rejected entries"
                        );
                    } else {
                        debug!(
                            n = applied.accepted,
                            registry_len = applied.registry_len,
                            "generator.demand POST ok"
                        );
                    }
                }
                (None, Some(e)) => {
                    // Walk the full anyhow chain so the operator sees
                    // the underlying reqwest cause (timeout vs.
                    // connection refused vs. DNS) — v13 logs only
                    // showed the top-level wrapper, leaving the root
                    // cause invisible.
                    let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
                    warn!(
                        error = %e,
                        chain = ?chain,
                        n = report.entries.len(),
                        attempts = MAX_ATTEMPTS,
                        "generator.demand POST failed after retries"
                    );
                }
                (None, None) => unreachable!("loop guarantees one branch"),
            }
        }
    }
}
