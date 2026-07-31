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
    control::ack_pending::{AckPendingEntry, AckPendingStore, TakeOutcome},
    control::flow_stats::{classify_endpoint, EndpointVerdict, FlowStats},
    peers::PeerRegistry,
    southbound::{
        orr::{
            HDR_ACK_ENDPOINT, HDR_KEY_DIGEST, HDR_KEY_ID, HDR_KEY_SIZE_BITS, HDR_MSG_TYPE,
            HDR_REQUEST_ID, HDR_SAE_ORIGIN, HDR_TIMESTAMP_MS, MSG_TYPE_DKMS_BUFFER,
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
    /// Peers DKMS, actualizables por la SDN. Antes era un `peer → orr_id`
    /// construido una vez aquí; ahora se consulta al registro para que un peer
    /// que entra en la red después llegue a este generador. Ver [`crate::peers`].
    peers: Arc<PeerRegistry>,
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
    /// Contadores de todo el ciclo emit → deliver → ACK, compartidos con el
    /// socket de ACK y con el pump de deliveries del ORR. Ver
    /// [`crate::control::flow_stats`].
    pub stats: Arc<FlowStats>,
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
        peers: Arc<PeerRegistry>,
        orr: Arc<OrrClient>,
        sdn_http: Arc<SdnHttpClient>,
        pool: Arc<BufferPool>,
        ack_pending: Arc<AckPendingStore>,
        demand_tracker: crate::demand_tracker::SharedDemandTracker,
        stats: Arc<FlowStats>,
    ) -> Self {
        // Aviso sobre la semilla del node.yml. El mapa efectivo lo lleva el
        // `PeerRegistry`, que ya descarta los peers sin `orr_id`.
        for (peer_id, pc) in &cfg.peers {
            if pc.transport == PeerTransport::Orr && pc.orr_id.is_none() {
                warn!(
                    peer = peer_id,
                    "generator: peer transport=orr sin orr_id configurado, se ignora"
                );
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
            peers,
            rates_enc: Arc::new(Mutex::new(HashMap::new())),
            qkd_avail: Arc::new(Mutex::new(HashMap::new())),
            buckets: Arc::new(Mutex::new(HashMap::new())),
            emit_counters: Arc::new(Mutex::new(HashMap::new())),
            stats,
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
        if me.peers.orr_map().is_empty() {
            // Antes esto era un `return` y el generador se apagaba para
            // siempre. Con peers que llegan de la SDN eso condenaría a un DKMS
            // que arranca solo: los bucles se lanzan igual y no hacen nada
            // mientras el mapa esté vacío.
            info!("generator: aún sin peers transport=orr; espero a que la SDN los mande");
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
        let probe = me.clone();
        tokio::spawn(async move {
            probe.check_ack_endpoint().await;
        });
        info!(
            my_dkms = %me.my_dkms_id,
            n_peers = me.peers.orr_map().len(),
            tick_ms = me.cfg.tick_ms,
            ack_timeout_ms = me.cfg.ack_timeout_ms,
            demand_refresh_ms = me.cfg.demand_refresh_ms,
            ack_endpoint = me.ack_endpoint.as_deref().unwrap_or("<none>"),
            "generator background started",
        );
        me
    }

    /// Comprueba que el `ack_endpoint` que anunciamos a los peers sirve
    /// para algo. Es la causa raíz más silenciosa del ciclo de claves: si
    /// la dirección anunciada no es enrutable, el peer manda su ACK a la
    /// nada (o, con `0.0.0.0`, a su propio socket) y aquí sólo se ve
    /// `ack_pending` creciendo hasta el tope y el reaper expirando.
    ///
    /// Dos comprobaciones:
    ///
    ///  1. **Estática**: ¿es `0.0.0.0` / `127.0.0.1`? Entonces ningún peer
    ///     de otra máquina podrá acusar recibo. `WARN` inmediato.
    ///  2. **Activa**: abrir un TCP contra la dirección anunciada. Si ni
    ///     siquiera desde esta máquina se puede, está mal la IP o el socket
    ///     no llegó a bindear. No prueba el cortafuegos del peer, pero
    ///     descarta la mitad de los casos sin salir del nodo.
    async fn check_ack_endpoint(self: Arc<Self>) {
        let Some(ep) = self.ack_endpoint.clone() else {
            warn!(
                "generator: sin ack_endpoint anunciado — los peers no podrán acusar recibo \
                 y todas las claves emitidas expirarán (define generator.ack_socket_addr \
                 o generator.ack_advertised_endpoint)"
            );
            return;
        };
        match classify_endpoint(&ep) {
            EndpointVerdict::Wildcard => warn!(
                ack_endpoint = %ep,
                "generator: el ack_endpoint anunciado es una wildcard; al conectarse, el peer \
                 la reinterpreta como localhost y su ACK acaba en su PROPIO socket. Pon \
                 generator.ack_advertised_endpoint (o advertise_ip) con la IP enrutable",
            ),
            EndpointVerdict::Loopback => warn!(
                ack_endpoint = %ep,
                "generator: el ack_endpoint anunciado es loopback; sólo vale si todos los DKMS \
                 comparten máquina. Desde otro host el ACK nunca llegará",
            ),
            EndpointVerdict::Routable => {}
        }
        // Damos margen a que el listener bindee antes de sondearlo.
        tokio::time::sleep(Duration::from_millis(500)).await;
        match tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(&ep))
            .await
        {
            Ok(Ok(_)) => {
                info!(ack_endpoint = %ep, "generator: ack_endpoint alcanzable (sonda local ok)")
            }
            Ok(Err(e)) => warn!(
                ack_endpoint = %ep, error = %e,
                "generator: el ack_endpoint anunciado NO es alcanzable ni desde esta máquina; \
                 ningún peer podrá acusar recibo",
            ),
            Err(_) => warn!(
                ack_endpoint = %ep,
                "generator: timeout conectando al ack_endpoint anunciado desde esta máquina",
            ),
        }
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
            // Unión de peers que aparecen en pool_snap, en peers_orr y en
            // los contadores de flujo — este último incluye peers de los
            // que sólo recibimos, que de otro modo serían invisibles.
            let mut all_peers: std::collections::BTreeSet<String> =
                pool_snap.iter().map(|(p, _, _)| p.clone()).collect();
            for p in self.peers.orr_map().keys() {
                all_peers.insert(p.clone());
            }
            for p in self.stats.peer_ids() {
                all_peers.insert(p);
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
                let f = self.stats.peer(&peer);
                let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
                info!(
                    peer = %peer,
                    enc = enc_len,
                    dec = dec_len,
                    ack_pending,
                    emit_total,
                    observed_keys_per_s = format!("{observed_rate:.1}"),
                    sdn_rate_keys_per_s = format!("{rate_sdn:.1}"),
                    // ── ida: yo genero para este peer ──────────────────
                    emitted = g(&f.emitted),
                    emit_failed = g(&f.emit_failed),
                    // ── vuelta: sus ACK ────────────────────────────────
                    acked = g(&f.acked),
                    expired = g(&f.expired),
                    ack_miss_peer = g(&f.ack_miss_unknown_peer),
                    ack_miss_key = g(&f.ack_miss_unknown_key),
                    enc_full = g(&f.enc_full),
                    // ── sentido contrario: él genera para mí ───────────
                    recv = g(&f.recv),
                    ack_sent = g(&f.ack_sent),
                    ack_send_failed = g(&f.ack_send_failed),
                    ack_no_endpoint = g(&f.ack_no_endpoint),
                    recv_corrupt = g(&f.recv_corrupt),
                    peer_ack_endpoint = self
                        .stats
                        .endpoint_of(&peer)
                        .unwrap_or_else(|| "<sin recibir>".into()),
                    "generator.state",
                );
                self.diagnose(&peer, &f, enc_len, ack_pending);
            }
        }
    }

    /// Traduce los contadores a una frase accionable. Se emite junto a
    /// `generator.state` sólo cuando hay algo roto, para que el operador no
    /// tenga que correlacionar doce números a mano.
    fn diagnose(
        &self,
        peer: &str,
        f: &crate::control::flow_stats::PeerFlow,
        enc_len: usize,
        ack_pending: usize,
    ) {
        // Sano: hay claves utilizables. Nada que decir.
        if enc_len > 0 {
            return;
        }
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let (emitted, acked, expired) = (g(&f.emitted), g(&f.acked), g(&f.expired));
        let (miss_peer, miss_key) = (g(&f.ack_miss_unknown_peer), g(&f.ack_miss_unknown_key));
        if emitted == 0 {
            // Sin emisión no hay nada que acusar. Puede ser normal en el
            // arranque; sólo molestamos si además hay pendientes.
            if ack_pending == 0 {
                debug!(peer, "generator.diag: aún sin emitir hacia este peer");
            }
            return;
        }
        let cause = if miss_peer > 0 {
            "sus ACK llegan con un `from` que no casa con el id con el que lo tengo \
             configurado (desajuste de identidad: revisa node_id del peer vs la clave \
             [peers.<id>] de mi node.yml)"
        } else if miss_key > 0 && acked == 0 {
            "sus ACK llegan TARDE: el reaper ya había expirado la clave. Sube \
             generator.ack_timeout_ms o baja la rate de emisión"
        } else if acked == 0 && expired > 0 {
            "emito y nada vuelve: o la clave no llega al peer (mira `recv` en SU log) \
             o su ACK no llega aquí (TCP hacia mi ack_endpoint bloqueado, o le anuncié \
             una dirección no enrutable)"
        } else {
            return;
        };
        warn!(
            peer,
            emitted,
            acked,
            expired,
            ack_miss_peer = miss_peer,
            ack_miss_key = miss_key,
            my_ack_endpoint = self.ack_endpoint.as_deref().unwrap_or("<none>"),
            "generator.diag: buffer_enc vacío — {cause}",
        );
    }

    /// Punto de entrada del ACK socket: el peer confirma que recibió la
    /// clave `key_id`. Movemos la entrada de `ack_pending[peer]` a
    /// `BufferPool.enc[peer]`. Devuelve `true` si se movió.
    pub fn on_ack(&self, peer_dkms_id: &str, key_id: &KeyId) -> bool {
        let entry = match self.ack_pending.take_diagnosed(peer_dkms_id, key_id) {
            TakeOutcome::Hit(e) => e,
            TakeOutcome::UnknownPeer => {
                self.stats.ack_miss_unknown_peer(peer_dkms_id, 1);
                // Rate-limitado a la primera y luego cada potencia de 2:
                // un desajuste de identidad produce un miss por CLAVE.
                let n = self
                    .stats
                    .peer(peer_dkms_id)
                    .ack_miss_unknown_peer
                    .load(Ordering::Relaxed);
                if n.is_power_of_two() {
                    warn!(
                        ack_from = peer_dkms_id,
                        key_id = %key_id,
                        misses = n,
                        pending_peers = ?self.ack_pending.peers(),
                        "generator.on_ack: ACK de un peer del que no espero nada. \
                         El `from` del ACK debe ser idéntico al id con el que emito \
                         (compara con pending_peers)",
                    );
                }
                return false;
            }
            TakeOutcome::UnknownKey => {
                self.stats.ack_miss_unknown_key(peer_dkms_id, 1);
                let n = self
                    .stats
                    .peer(peer_dkms_id)
                    .ack_miss_unknown_key
                    .load(Ordering::Relaxed);
                if n.is_power_of_two() {
                    warn!(
                        peer = peer_dkms_id,
                        key_id = %key_id,
                        misses = n,
                        ack_timeout_ms = self.cfg.ack_timeout_ms,
                        "generator.on_ack: ACK tardío o duplicado — la clave ya no estaba \
                         en ack_pending (probablemente la expiró el reaper)",
                    );
                }
                return false;
            }
        };
        let buf = self.pool.for_peer(peer_dkms_id);
        let grade = entry.grade;
        let key = TransportKey {
            id: key_id.clone(),
            bytes: entry.bytes,
        };
        if let Err(rejected) = buf.enc(grade).try_push(key) {
            // Buffer lleno: la clave se descarta (zeroize en Drop).
            self.stats.enc_full(peer_dkms_id, 1);
            warn!(
                peer = peer_dkms_id,
                key_id = %rejected.id,
                "generator.on_ack: buffer_enc full, key dropped",
            );
            return false;
        }
        let n = self
            .counter_for(peer_dkms_id)
            .fetch_add(1, Ordering::Relaxed);
        self.stats.acked(peer_dkms_id, 1);
        if n == 0 {
            // Hito: el ciclo emit→ACK se cerró por primera vez con este
            // peer. Sin esta línea, un despliegue sano y uno roto se ven
            // igual durante los primeros 5 s.
            info!(
                peer = peer_dkms_id,
                grade = ?grade,
                "generator: primer ACK de este peer — el ciclo de claves está cerrado",
            );
        }
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
            .peers
            .orr_map()
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
        let orr_map = self.peers.orr_map();
        let Some(dest_orr_id) = orr_map.get(peer_dkms_id).cloned() else {
            return Ok(()); // no debería pasar (filtrado antes)
        };
        if self.ack_endpoint.is_none() {
            self.stats.ack_no_endpoint(peer_dkms_id, 1);
        }
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
        // Integridad extremo a extremo del material: el enlace QKC cifra con
        // OTP sin MAC, así que sin esto una corrupción por debajo se guarda
        // como clave buena y las dos puntas acaban con claves distintas.
        header.insert(
            HDR_KEY_DIGEST.into(),
            crate::southbound::orr::key_digest(&key_id_str, &bytes),
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
            self.stats.emit_failed(peer_dkms_id, 1);
            return Err(anyhow::anyhow!(e.to_string()));
        }
        let n = self
            .stats
            .peer(peer_dkms_id)
            .emitted
            .fetch_add(1, Ordering::Relaxed);
        if n == 0 {
            info!(
                peer = peer_dkms_id,
                dest_orr = %dest_orr_id,
                ack_endpoint = self.ack_endpoint.as_deref().unwrap_or("<none>"),
                "generator: primera clave emitida hacia este peer",
            );
        }
        Ok(())
    }

    async fn run_reaper_loop(self: Arc<Self>) {
        let period = Duration::from_millis(self.cfg.ack_reaper_ms);
        loop {
            // Por peer, no agregado: un total no distingue "se pierde todo
            // hacia un peer" de "se pierde un poco hacia todos".
            let removed = self.ack_pending.reap_expired_by_peer(Instant::now());
            if !removed.is_empty() {
                let total: usize = removed.iter().map(|(_, n)| n).sum();
                for (peer, n) in &removed {
                    self.stats.expired(peer, *n as u64);
                }
                warn!(
                    removed = total,
                    by_peer = ?removed,
                    ack_timeout_ms = self.cfg.ack_timeout_ms,
                    "generator.ack_reaper: expired pending keys",
                );
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
        let orr_map = self.peers.orr_map();
        let entries: Vec<CommodityDemand> = orr_map
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
                    dst_dkms: peer.to_string(),
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
