//! Buffers de claves por enlace.
//!
//! Cada enlace QKC↔QKC tiene un `KeyStore` con:
//!
//! * **Buffer ENC** — claves recién pedidas vía `enc_keys` al quditto
//!   compartido. Listo para cifrar mensajes salientes. Cola lock-free.
//! * **Buffer DEC** — claves esperadas que el peer va a usar para
//!   mandarnos cosas. El peer nos avisó con `FRAME_KEY_IDS_NOTIFY` y
//!   nosotros las pedimos al quditto vía `dec_keys`. Map `id → material`.
//! * **Worker ENC** — background task que rellena el ENC cuando baja
//!   de `REFILL_THRESHOLD`. Tras meter las claves en el buffer manda
//!   un `FRAME_KEY_IDS_NOTIFY` al peer.
//! * **Worker DEC** — background task que reacciona a `notify_remote_enc`,
//!   pide a `dec_keys` y rellena el DEC.
//!
//! El hot path del relay/encrypt **nunca** hace HTTP: solo `take_enc()`
//! y `lookup_dec()` (memoria). Si por race del warm-up no encuentra una
//! clave en DEC, el caller puede caer a un fallback HTTP on-demand.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use parking_lot::Mutex;
use tokio::sync::Notify;
use tracing::{debug, info, warn};
use uuid::Uuid;
use wire::{encode_notify_payload, Frame, FRAME_KEY_IDS_NOTIFY};
use zeroize::Zeroizing;

use crate::{
    kme::{KeySource, KmeStock, OtpKey},
    rate_estimator::{RateEstimator, RateReport},
    transport::peer_client::PeerOut,
};

/// Cantidad objetivo de claves en el buffer ENC.
pub const BUFFER_TARGET: usize = 2048;
/// Umbral bajo el cual el worker dispara un refill.
///
/// Se mantiene bajo (256) para que el worker reaccione antes y no
/// haya largos periodos de buffer drenado durante carga. Con R0 bajo
/// (p.ej. 2000 keys/s) además interactúa mal tener un umbral grande:
/// el worker espera demasiado entre refills.
pub const REFILL_THRESHOLD: usize = 256;
/// Lote por refill. Igual a `max_key_per_request` del quditto (128)
/// para que cada request HTTP se sirva COMPLETA en el tick siguiente
/// (200 keys/100 ms a R0=2000) sin partials → menos overhead.
pub const REFILL_BATCH: u32 = 128;

/// Tope de IDs pendientes de `dec_keys` antes de aplastar. Si el peer
/// notifica más rápido de lo que podemos absorber, el remanente queda
/// en la cola interna. Lo mantenemos pequeño para igual razón que
/// `REFILL_BATCH`: lotes grandes con quditto sin stock generan
/// roundtrips lentos.
const DEC_BATCH_MAX: usize = 128;

/// Cadencia del sondeo de `/status` del KME que alimenta el estimador de
/// tasa (ver `rate_estimator.rs`). Rápida mientras no hay estimación — la
/// ventana limpia del arranque (el fill inicial drena el KME) dura pocos
/// segundos y es la que da el primer número — y 1 Hz después.
const STOCK_POLL: Duration = Duration::from_millis(1000);
const STOCK_POLL_COLD: Duration = Duration::from_millis(250);
/// Marca alta del stock del KME (fracción de `max_key_count`) a partir de la
/// cual el sondeador *banca*: se lleva lotes al anillo ENC para que el KME no
/// toque su techo. Al techo el KME descarta (o pausa) producción — material
/// destruido Y tasa inobservable; bancado, ni se pierde ni deja de medirse.
const BANK_HIGH_WATER_NUM: u64 = 3;
const BANK_HIGH_WATER_DEN: u64 = 4;
/// Hueco del anillo ENC que el banking nunca invade: el refill por demanda
/// (`enc_low`) tiene que poder trabajar aunque estemos bancando.
const BANK_RING_HEADROOM: usize = 512;
/// Lotes de banking por sondeo, como máximo (16 × 128 = 2048 claves/sondeo:
/// por encima de cualquier R razonable, sin convertir el sondeo en un bucle
/// infinito si el KME produce más de lo que cabe).
const MAX_BANK_ROUNDS: u32 = 16;

pub struct KeyStore {
    /// Buffer FIFO de claves listas para cifrar (mías a cuenta del peer).
    enc: ArrayQueue<OtpKey>,
    /// Mapa de claves esperadas para descifrar (las del peer hacia mí).
    dec: DashMap<Uuid, Zeroizing<Vec<u8>>>,

    /// Fuente de claves del enlace (quditto-QKD o PQC). El KeyStore es
    /// agnóstico al tipo: solo llama a `enc_keys`/`dec_keys`.
    kme: Arc<dyn KeySource>,

    /// Para mandar `FRAME_KEY_IDS_NOTIFY` al peer cuando rellenamos
    /// nuestro buffer ENC.
    peer_out: Arc<PeerOut>,
    /// ID del peer (sender_id de nuestros frames hacia él) y dir TCP.
    peer_id: u32,
    /// Autenticación del enlace, compartida con el camino de datos. Sella los
    /// `FRAME_KEY_IDS_NOTIFY` con `session ‖ counter ‖ tag`. HMAC simétrico ⇒
    /// resistente a cuántico, y 48 B de trailer frente a los 3309 de una firma
    /// ML-DSA, que en un frame tan frecuente no sale a cuenta.
    /// `None` ⇒ notify sin autenticar (comportamiento histórico).
    frame_auth: Option<Arc<crate::frame_auth::LinkFrameAuth>>,
    peer_addr: String,
    /// Nuestro propio ID (sender_id del NOTIFY).
    my_id: u32,
    /// `key_size_bits` que vamos a anunciar en el frame de NOTIFY (no
    /// es funcional para el procesado del peer, pero lo dejamos
    /// coherente con el resto del wire).
    key_size_bits: u16,

    /// Signal: "buffer ENC bajo, hay que refill".
    enc_low: Arc<Notify>,
    /// Cola de IDs pendientes de pedir al quditto vía `dec_keys`.
    dec_pending: Mutex<Vec<Uuid>>,
    /// Signal: "hay IDs pendientes en `dec_pending`".
    dec_request: Arc<Notify>,

    /// Signal: "el worker DEC acaba de insertar claves" — `notify_waiters`
    /// despierta a TODOS los frames que estén esperando una clave que
    /// todavía no llegó al `DashMap`. Sustituye al antiguo fallback HTTP,
    /// que era inviable porque el quditto ya entregó la clave al worker
    /// y un `dec_keys` posterior devuelve 404.
    dec_inserted: Arc<Notify>,
    /// Contador monotónico de inserts DEC. El relay lo lee antes de
    /// crear el `Notified` future para detectar un insert que ocurriese
    /// entre la `lookup_dec` que falló y la inscripción como waiter.
    dec_inserted_seq: AtomicU64,

    /// Análogo a `dec_inserted` pero para ENC. Permite que el hot path
    /// se backpressuree contra el worker en vez de spawnear miles de
    /// `enc_keys` HTTP paralelos cuando el buffer se vacía.
    enc_inserted: Arc<Notify>,
    enc_inserted_seq: AtomicU64,

    /// Estimador de la tasa de generación del enlace (solo mide algo en
    /// fuentes con almacén — QKD; en PQC nunca recibe muestras). Lo alimentan
    /// los workers (entregas) y `stock_rate_loop` (nivel del KME); lo lee el
    /// anunciante a la SDN vía [`KeyStore::rate_report`].
    estimator: Arc<RateEstimator>,

    // Stats (lecturas en hot path, escrituras desde workers).
    n_enc_taken: AtomicU64,
    n_dec_lookups: AtomicU64,
    n_dec_misses: AtomicU64,
    n_refills_enc: AtomicU64,
    /// Claves ENC que el KME ya entregó (material QKD pagado) y no cupieron
    /// en el anillo. "No debería pasar" — y por eso se cuenta.
    n_enc_dropped_full: AtomicU64,
    /// `enc_keys` al KME que fallaron (KME caído, TLS, timeout).
    n_refill_enc_failed: AtomicU64,
    /// NOTIFY que no salieron porque la cola hacia el peer estaba llena.
    n_notify_dropped: AtomicU64,
    n_refills_dec: AtomicU64,
    n_wait_enc_called: AtomicU64,
    n_wait_enc_succeeded: AtomicU64,
    n_wait_enc_timeouts: AtomicU64,
    n_wait_dec_called: AtomicU64,
    n_wait_dec_succeeded: AtomicU64,
    n_wait_dec_timeouts: AtomicU64,
}

impl KeyStore {
    pub fn new(
        kme: Arc<dyn KeySource>,
        peer_out: Arc<PeerOut>,
        peer_id: u32,
        peer_addr: String,
        my_id: u32,
        key_size_bits: u32,
        frame_auth: Option<Arc<crate::frame_auth::LinkFrameAuth>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            frame_auth,
            enc: ArrayQueue::new(BUFFER_TARGET * 2),
            dec: DashMap::with_capacity(BUFFER_TARGET * 2),
            kme,
            peer_out,
            peer_id,
            peer_addr,
            my_id,
            key_size_bits: key_size_bits as u16,
            enc_low: Arc::new(Notify::new()),
            dec_pending: Mutex::new(Vec::with_capacity(DEC_BATCH_MAX)),
            dec_request: Arc::new(Notify::new()),
            dec_inserted: Arc::new(Notify::new()),
            dec_inserted_seq: AtomicU64::new(0),
            enc_inserted: Arc::new(Notify::new()),
            enc_inserted_seq: AtomicU64::new(0),
            estimator: Arc::new(RateEstimator::new()),
            n_enc_taken: AtomicU64::new(0),
            n_dec_lookups: AtomicU64::new(0),
            n_dec_misses: AtomicU64::new(0),
            n_refills_enc: AtomicU64::new(0),
            n_enc_dropped_full: AtomicU64::new(0),
            n_refill_enc_failed: AtomicU64::new(0),
            n_notify_dropped: AtomicU64::new(0),
            n_refills_dec: AtomicU64::new(0),
            n_wait_enc_called: AtomicU64::new(0),
            n_wait_enc_succeeded: AtomicU64::new(0),
            n_wait_enc_timeouts: AtomicU64::new(0),
            n_wait_dec_called: AtomicU64::new(0),
            n_wait_dec_succeeded: AtomicU64::new(0),
            n_wait_dec_timeouts: AtomicU64::new(0),
        })
    }

    /// Lanza los workers de background. Llamar UNA vez por keystore al
    /// boot. Dispara también un prefetch inicial.
    pub fn spawn_workers(self: &Arc<Self>) {
        // ENC worker.
        let s = self.clone();
        tokio::spawn(async move { s.enc_refill_loop().await });
        // DEC worker.
        let s = self.clone();
        tokio::spawn(async move { s.dec_refill_loop().await });
        // Sondeo de stock del KME → estimador de tasa + banking. Si la
        // fuente no tiene almacén (PQC), la primera llamada devuelve
        // `Ok(None)` y la task muere sola.
        let s = self.clone();
        tokio::spawn(async move { s.stock_rate_loop().await });
        // Prefetch inicial.
        self.enc_low.notify_one();
    }

    // ─── Hot path: lo llaman los handlers de frames ────────────────

    /// Saca UNA clave del buffer ENC. `None` si está vacío
    /// (caller decide: esperar, fallback HTTP, o error).
    #[inline]
    pub fn take_enc(&self) -> Option<OtpKey> {
        let k = self.enc.pop();
        if k.is_some() {
            self.n_enc_taken.fetch_add(1, Ordering::Relaxed);
            // Si bajamos del threshold, signal al worker.
            if self.enc.len() < REFILL_THRESHOLD {
                self.enc_low.notify_one();
            }
        }
        k
    }

    /// Saca N claves del buffer ENC. Si no hay suficientes, devuelve
    /// las que pueda (el caller llama después a [`wait_enc_batch`] para
    /// esperar al worker).
    pub fn take_enc_batch(&self, n: usize) -> Vec<OtpKey> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            match self.enc.pop() {
                Some(k) => out.push(k),
                None => break,
            }
        }
        if !out.is_empty() {
            self.n_enc_taken
                .fetch_add(out.len() as u64, Ordering::Relaxed);
        }
        // Avisa al worker SIEMPRE que el buffer esté bajo, incluso si
        // no logramos sacar nada (caller con buffer vacío).
        if self.enc.len() < REFILL_THRESHOLD {
            self.enc_low.notify_one();
        }
        out
    }

    /// Como `take_enc_batch` pero **espera** al worker si el buffer no
    /// puede servir todas las claves. Devuelve `Ok` con exactamente `n`
    /// claves o un `KeyWaitTimeout` si el deadline expira primero.
    ///
    /// Sustituye al antiguo fallback HTTP `enc_keys`, que duplicaba
    /// peticiones a quditto bajo carga.
    pub async fn wait_enc_batch(
        &self,
        n: usize,
        timeout: Duration,
    ) -> Result<Vec<OtpKey>, KeyWaitTimeout> {
        self.n_wait_enc_called.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        let mut out = self.take_enc_batch(n);
        while out.len() < n {
            let need = n - out.len();
            let seq_before = self.enc_inserted_seq.load(Ordering::SeqCst);

            // Re-try la cola por si entró algo entre el último pop y aquí.
            let extra = self.take_enc_batch(need);
            if !extra.is_empty() {
                out.extend(extra);
                if out.len() == n {
                    break;
                }
            }

            let now = Instant::now();
            if now >= deadline {
                self.n_wait_enc_timeouts.fetch_add(1, Ordering::Relaxed);
                debug!(
                    peer = self.peer_id,
                    missing = n - out.len(),
                    timeout_ms = timeout.as_millis() as u64,
                    enc_len = self.enc.len(),
                    "keystore.wait_enc_batch.timeout"
                );
                return Err(KeyWaitTimeout {
                    missing: n - out.len(),
                });
            }
            let remaining = deadline - now;

            let notified = self.enc_inserted.notified();
            tokio::pin!(notified);
            // `enable` inscribe el future en la lista de waiters ANTES
            // de la siguiente carga del seq → así no se nos escapa un
            // `notify_waiters` ocurrido justo en este instante.
            notified.as_mut().enable();

            // Si entre el `seq_before` y el `enable` hubo un insert, ya
            // no necesitamos esperar — re-loop a probar la cola.
            if self.enc_inserted_seq.load(Ordering::SeqCst) != seq_before {
                continue;
            }

            // Asegúrate de que el worker esté despierto: si nadie le
            // ha hecho `notify_one` y el buffer está bajo, despiértalo.
            self.enc_low.notify_one();

            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    self.n_wait_enc_timeouts.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        peer = self.peer_id,
                        missing = n - out.len(),
                        timeout_ms = timeout.as_millis() as u64,
                        enc_len = self.enc.len(),
                        "keystore.wait_enc_batch.timeout"
                    );
                    return Err(KeyWaitTimeout {
                        missing: n - out.len(),
                    });
                }
            }
        }
        self.n_wait_enc_succeeded.fetch_add(1, Ordering::Relaxed);
        // FIFO cascade: si quedan keys en el buffer después de servirnos,
        // despierta al siguiente waiter (notify_one es FIFO en tokio).
        // Sin esto, sólo el primer waiter de cada refill obtiene servicio
        // → resto se quedan dormidos hasta el próximo refill.
        if !self.enc.is_empty() {
            self.enc_inserted.notify_one();
        }
        Ok(out)
    }

    /// Busca y CONSUME la clave por `key_id` en el buffer DEC.
    /// `None` si no la tiene → caller llama a [`wait_dec`] para esperar
    /// al worker. Consumir = mover fuera del mapa (un pad no se usa dos
    /// veces); si el caller luego falla el batch, la clave está quemada —
    /// ver la nota en `relay::lookup_or_fetch_dec`.
    #[inline]
    pub fn lookup_dec(&self, id: &Uuid) -> Option<Zeroizing<Vec<u8>>> {
        self.n_dec_lookups.fetch_add(1, Ordering::Relaxed);
        match self.dec.remove(id) {
            Some((_, v)) => Some(v),
            None => {
                self.n_dec_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Como `lookup_dec` pero **espera** al `dec_refill_loop` si la
    /// clave todavía no está en el `DashMap`. El antiguo fallback HTTP
    /// `dec_keys` era inviable porque el quditto ya entregó la clave
    /// al worker (responde 404). Devolvemos `Err` si el deadline expira.
    pub async fn wait_dec(
        &self,
        id: &Uuid,
        timeout: Duration,
    ) -> Result<Zeroizing<Vec<u8>>, KeyWaitTimeout> {
        self.n_wait_dec_called.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        loop {
            let seq_before = self.dec_inserted_seq.load(Ordering::SeqCst);
            if let Some(v) = self.lookup_dec(id) {
                self.n_wait_dec_succeeded.fetch_add(1, Ordering::Relaxed);
                return Ok(v);
            }

            let now = Instant::now();
            if now >= deadline {
                self.n_wait_dec_timeouts.fetch_add(1, Ordering::Relaxed);
                debug!(
                    peer = self.peer_id,
                    id = %id,
                    timeout_ms = timeout.as_millis() as u64,
                    dec_len = self.dec.len(),
                    "keystore.wait_dec.timeout"
                );
                return Err(KeyWaitTimeout { missing: 1 });
            }
            let remaining = deadline - now;

            let notified = self.dec_inserted.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.dec_inserted_seq.load(Ordering::SeqCst) != seq_before {
                continue;
            }

            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => {
                    self.n_wait_dec_timeouts.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        peer = self.peer_id,
                        id = %id,
                        timeout_ms = timeout.as_millis() as u64,
                        dec_len = self.dec.len(),
                        "keystore.wait_dec.timeout"
                    );
                    return Err(KeyWaitTimeout { missing: 1 });
                }
            }
        }
    }

    // ─── Llamado por peer_server al recibir FRAME_KEY_IDS_NOTIFY ──

    /// El peer acaba de pedir estos IDs a su buffer enc. Nosotros los
    /// añadimos a la cola de `dec_pending` y notificamos al worker
    /// para que haga el `dec_keys` al quditto.
    pub fn notify_remote_enc(&self, ids: Vec<Uuid>) {
        // Para el estimador, estas claves YA salieron del almacén (las drenó
        // el `enc_keys` del peer hace milisegundos). Contarlas al completar
        // nuestro `dec_keys` — segundos por detrás durante el fill — dejaba
        // su ΔS sin compensar: −15 % medidos en vivo (2026-09-01).
        self.estimator.on_peer_drained(ids.len());
        let mut g = self.dec_pending.lock();
        g.extend(ids);
        drop(g);
        self.dec_request.notify_one();
    }

    // ─── Workers de background ─────────────────────────────────────

    async fn enc_refill_loop(self: Arc<Self>) {
        // Con raíz per-época, no pedir claves (ni por tanto emitir el primer
        // NOTIFY) hasta que el handshake instale la primera época: un NOTIFY
        // en claro contra un peer con raíz se descarta fail-closed y sus
        // claves quedan huérfanas. En QKD el KME local gana la carrera al
        // handshake sin esta espera; en PQC es un no-op (la fuente de claves
        // ES el handshake). Ver `LinkFrameAuth::wait_send_root`.
        if let Some(fa) = &self.frame_auth {
            fa.wait_send_root(Duration::from_secs(30)).await;
        }
        loop {
            self.enc_low.notified().await;
            // Refill mientras el buffer esté bajo. Una vez arriba,
            // volvemos a esperar el siguiente notify.
            while self.enc.len() < REFILL_THRESHOLD {
                let space = self.enc.capacity() - self.enc.len();
                let batch = (space as u32).clamp(1, REFILL_BATCH);
                match self.kme.enc_keys(batch).await {
                    Ok(keys) => {
                        self.absorb_enc_keys(keys);
                        debug!(
                            peer = self.peer_id,
                            batch = batch,
                            enc_len = self.enc.len(),
                            "keystore.enc_refill"
                        );
                    }
                    Err(e) => {
                        // Bucle a 100 ms mientras el KME esté caído: el
                        // contador va a `keystore.levels`, el log habla en
                        // las potencias de dos.
                        let n = self.n_refill_enc_failed.fetch_add(1, Ordering::Relaxed);
                        if common::log_throttle::nth_is_loud(n) {
                            warn!(
                                peer = self.peer_id,
                                error = %e,
                                failures = n + 1,
                                "keystore.enc_refill failed"
                            );
                        }
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    /// Mete un lote recién entregado por el KME en el anillo ENC, cuenta,
    /// despierta a un waiter y manda el NOTIFY al peer. Común al refill por
    /// demanda y al banking del sondeador de stock.
    fn absorb_enc_keys(&self, keys: Vec<OtpKey>) {
        self.estimator.on_delivered(keys.len());
        self.n_refills_enc.fetch_add(1, Ordering::Relaxed);
        let mut ids_raw = Vec::with_capacity(keys.len());
        for k in keys {
            ids_raw.push(*k.key_id.as_bytes());
            // Si el anillo está lleno (no debería) la clave
            // se pierde: material QKD ya pagado, así que
            // queda contado en `keystore.levels`.
            if self.enc.push(k).is_err() {
                self.n_enc_dropped_full.fetch_add(1, Ordering::Relaxed);
            }
        }
        // Despierta UN solo waiter (notify_one es FIFO en
        // tokio). El waiter que se despierta, tras coger
        // sus claves, hace cascade `notify_one` al siguiente
        // si quedan keys. Esto evita el thundering herd
        // del antiguo `notify_waiters` y da fairness real.
        self.enc_inserted_seq.fetch_add(1, Ordering::SeqCst);
        self.enc_inserted.notify_one();
        // NOTIFY al peer.
        self.send_notify(&ids_raw);
    }

    /// Sondeo periódico del `/status` del KME: alimenta el estimador de tasa
    /// y banca lotes al anillo ENC cuando el stock roza su techo (donde el
    /// KME descarta o pausa producción — material perdido y tasa invisible).
    /// Ver `rate_estimator.rs` para el porqué completo.
    async fn stock_rate_loop(self: Arc<Self>) {
        // Mismo gate que el enc loop: el banking emite NOTIFYs, y con raíz
        // per-época un NOTIFY en claro se descarta fail-closed en el peer.
        if let Some(fa) = &self.frame_auth {
            fa.wait_send_root(Duration::from_secs(30)).await;
        }
        let mut n_errs: u64 = 0;
        loop {
            match self.kme.stock().await {
                // Fuente sin almacén (PQC): no hay nada que medir, jamás.
                Ok(None) => return,
                Ok(Some(s)) => {
                    self.estimator.on_stock(s.stored, s.max, Instant::now());
                    self.bank_from(s).await;
                }
                Err(e) => {
                    self.estimator.on_stock_error();
                    if common::log_throttle::nth_is_loud(n_errs) {
                        warn!(
                            peer = self.peer_id,
                            error = %e,
                            failures = n_errs + 1,
                            "keystore.stock_poll failed"
                        );
                    }
                    n_errs += 1;
                }
            }
            // Cadencia rápida mientras no hay estimación: la ventana limpia
            // del arranque (el fill inicial drena el KME) dura pocos
            // segundos y es la que da el primer número.
            let delay = if self.estimator.report().is_some() {
                STOCK_POLL
            } else {
                STOCK_POLL_COLD
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// Banca lotes mientras el stock del KME siga sobre la marca alta y el
    /// anillo tenga hueco (dejando [`BANK_RING_HEADROOM`] para el refill por
    /// demanda). `stored` se descuenta en local entre lotes; el siguiente
    /// sondeo lo re-ancla.
    async fn bank_from(&self, s: KmeStock) {
        let mut stored = s.stored;
        for _ in 0..MAX_BANK_ROUNDS {
            let Some(batch) = bank_batch_size(stored, s.max, self.enc.len(), self.enc.capacity())
            else {
                return;
            };
            match self.kme.enc_keys(batch).await {
                Ok(keys) => {
                    let n = keys.len() as u64;
                    self.absorb_enc_keys(keys);
                    debug!(
                        peer = self.peer_id,
                        batch = n,
                        enc_len = self.enc.len(),
                        kme_stored = stored,
                        "keystore.bank"
                    );
                    stored = stored.saturating_sub(n);
                    if n < u64::from(batch) {
                        return; // el KME dio menos de lo pedido: no insistas
                    }
                }
                Err(_) => return, // el error ya lo cuenta/loguea el refill normal
            }
        }
    }

    async fn dec_refill_loop(self: Arc<Self>) {
        loop {
            self.dec_request.notified().await;
            loop {
                let ids = {
                    let mut g = self.dec_pending.lock();
                    if g.is_empty() {
                        break;
                    }
                    // Tope por batch para no enviar HTTPs gigantes.
                    let take = g.len().min(DEC_BATCH_MAX);
                    g.drain(..take).collect::<Vec<_>>()
                };
                match self.kme.dec_keys(&ids).await {
                    Ok(keys) => {
                        // OJO: estas claves NO se cuentan aquí para el
                        // estimador — ya se contaron al llegar el NOTIFY
                        // (`notify_remote_enc`), que es cuando de verdad
                        // salieron del almacén. Contarlas dos veces inflaría
                        // la tasa.
                        self.n_refills_dec.fetch_add(1, Ordering::Relaxed);
                        let n = keys.len();
                        for k in keys {
                            self.dec.insert(k.key_id, k.material);
                        }
                        // Despierta a `wait_dec` para todos los frames
                        // que estuviesen esperando estas claves.
                        self.dec_inserted_seq.fetch_add(1, Ordering::SeqCst);
                        self.dec_inserted.notify_waiters();
                        debug!(
                            peer = self.peer_id,
                            received = n,
                            dec_len = self.dec.len(),
                            "keystore.dec_refill"
                        );
                    }
                    Err(e) => {
                        warn!(
                            peer = self.peer_id,
                            ids = ids.len(),
                            error = %e,
                            "keystore.dec_refill failed"
                        );
                    }
                }
            }
        }
    }

    fn send_notify(&self, ids_raw: &[[u8; 16]]) {
        // El NOTIFY decide qué `key_ID` pide el peer a su KME: forjarlo permite
        // descuadrar el consumo de claves de un enlace QKD, que es el único
        // plano del enlace que el propio QKD no protege. Con raíz de enlace va
        // sellado por el MISMO mecanismo que los frames de datos, así que
        // hereda el contador y la ventana anti-replay — antes llevaba un HMAC
        // propio con `epoch = 0` y sin contador, y un NOTIFY capturado se
        // reinyectaba tal cual.
        let mut frame = Frame::empty(FRAME_KEY_IDS_NOTIFY);
        frame.sender_id = self.my_id;
        frame.receiver_id = self.peer_id;
        frame.dest_final = self.peer_id;
        frame.key_size_bits = self.key_size_bits;
        frame.payload = encode_notify_payload(ids_raw);
        if let Some(fa) = &self.frame_auth {
            fa.seal(&mut frame);
        }
        let ok = self.peer_out.send(self.peer_id, &self.peer_addr, frame);
        if !ok {
            let n = self.n_notify_dropped.fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    peer = self.peer_id,
                    dropped = n + 1,
                    "keystore.send_notify queue full"
                );
            }
        }
    }

    /// Estimación de la tasa de generación del enlace, si la hay (fuentes
    /// con almacén — QKD — y al menos una ventana válida). La anuncia el
    /// `SdnAnnouncer` en cada latido.
    pub fn rate_report(&self) -> Option<RateReport> {
        self.estimator.report()
    }

    /// Diagnóstico — snapshot de niveles.
    pub fn levels(&self) -> KeyStoreLevels {
        let rate = self.estimator.report();
        KeyStoreLevels {
            rate_est_kps: rate.map(|r| r.keys_per_s),
            rate_quality: rate.map(|r| r.quality.as_str()),
            enc_buffered: self.enc.len(),
            dec_buffered: self.dec.len(),
            enc_taken: self.n_enc_taken.load(Ordering::Relaxed),
            dec_lookups: self.n_dec_lookups.load(Ordering::Relaxed),
            dec_misses: self.n_dec_misses.load(Ordering::Relaxed),
            refills_enc: self.n_refills_enc.load(Ordering::Relaxed),
            enc_dropped_full: self.n_enc_dropped_full.load(Ordering::Relaxed),
            refill_enc_failed: self.n_refill_enc_failed.load(Ordering::Relaxed),
            notify_dropped: self.n_notify_dropped.load(Ordering::Relaxed),
            refills_dec: self.n_refills_dec.load(Ordering::Relaxed),
            wait_enc_called: self.n_wait_enc_called.load(Ordering::Relaxed),
            wait_enc_succeeded: self.n_wait_enc_succeeded.load(Ordering::Relaxed),
            wait_enc_timeouts: self.n_wait_enc_timeouts.load(Ordering::Relaxed),
            wait_dec_called: self.n_wait_dec_called.load(Ordering::Relaxed),
            wait_dec_succeeded: self.n_wait_dec_succeeded.load(Ordering::Relaxed),
            wait_dec_timeouts: self.n_wait_dec_timeouts.load(Ordering::Relaxed),
        }
    }
}

/// Devuelto por `wait_dec` / `wait_enc_batch` cuando el deadline
/// expira antes de que el worker termine de servir las claves.
/// El relay lo convierte en `QkcError::KeyWaitTimeout`.
#[derive(Debug, Clone, Copy)]
pub struct KeyWaitTimeout {
    pub missing: usize,
}

/// Cuánto bancar en este lote, o `None` si no toca: el stock del KME sigue
/// bajo la marca alta, o el anillo no tiene hueco por encima de la reserva
/// del refill por demanda. Función pura para poder testearla sin KME.
fn bank_batch_size(stored: u64, max: u64, ring_len: usize, ring_cap: usize) -> Option<u32> {
    if max == 0 || stored < max / BANK_HIGH_WATER_DEN * BANK_HIGH_WATER_NUM {
        return None;
    }
    let space = ring_cap.saturating_sub(ring_len);
    if space <= BANK_RING_HEADROOM {
        return None;
    }
    let batch = (space - BANK_RING_HEADROOM)
        .min(REFILL_BATCH as usize)
        .min(stored as usize);
    u32::try_from(batch).ok().filter(|b| *b > 0)
}

#[derive(Debug, Clone)]
pub struct KeyStoreLevels {
    /// Tasa de generación estimada del enlace (claves/s), si se pudo medir.
    pub rate_est_kps: Option<f64>,
    /// `measured` / `floor` / `unavailable` — ver `rate_estimator.rs`.
    pub rate_quality: Option<&'static str>,
    pub enc_buffered: usize,
    pub dec_buffered: usize,
    pub enc_taken: u64,
    pub dec_lookups: u64,
    pub dec_misses: u64,
    pub refills_enc: u64,
    pub enc_dropped_full: u64,
    pub refill_enc_failed: u64,
    pub notify_dropped: u64,
    pub refills_dec: u64,
    pub wait_enc_called: u64,
    pub wait_enc_succeeded: u64,
    pub wait_enc_timeouts: u64,
    pub wait_dec_called: u64,
    pub wait_dec_succeeded: u64,
    pub wait_dec_timeouts: u64,
}

/// Logger periódico de niveles (útil para diagnóstico durante stress).
pub fn spawn_level_logger(stores: Vec<(u32, Arc<KeyStore>)>, every: Duration) {
    if stores.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(every);
        ticker.tick().await; // skip inicial
        loop {
            ticker.tick().await;
            for (peer, ks) in &stores {
                let l = ks.levels();
                info!(
                    peer,
                    rate = l.rate_est_kps.map(|r| format!("{r:.1}")),
                    rate_q = l.rate_quality,
                    enc = l.enc_buffered,
                    dec = l.dec_buffered,
                    taken = l.enc_taken,
                    misses = l.dec_misses,
                    wenc = l.wait_enc_called,
                    wenc_to = l.wait_enc_timeouts,
                    wdec = l.wait_dec_called,
                    wdec_to = l.wait_dec_timeouts,
                    enc_drop = l.enc_dropped_full,
                    refill_fail = l.refill_enc_failed,
                    notify_drop = l.notify_dropped,
                    "keystore.levels"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Un KME que devuelve más claves de las pedidas: la única forma de que
    /// el anillo ENC se desborde, porque el refill pide justo el hueco.
    struct OverflowingKme;

    #[async_trait::async_trait]
    impl KeySource for OverflowingKme {
        async fn enc_keys(&self, number: u32) -> crate::error::Result<Vec<OtpKey>> {
            Ok((0..number as usize + BUFFER_TARGET * 2)
                .map(|_| OtpKey {
                    key_id: Uuid::new_v4(),
                    material: Zeroizing::new(vec![0u8; 32]),
                })
                .collect())
        }
        async fn dec_keys(&self, _ids: &[Uuid]) -> crate::error::Result<Vec<OtpKey>> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn bank_batch_respects_high_water_and_ring_headroom() {
        let cap = BUFFER_TARGET * 2;
        // Bajo la marca alta (3/4 de 8192 = 6144): no se banca.
        assert_eq!(bank_batch_size(1000, 8192, 0, cap), None);
        assert_eq!(bank_batch_size(6143, 8192, 0, cap), None);
        // Sobre la marca con anillo vacío: lote completo.
        assert_eq!(bank_batch_size(8000, 8192, 0, cap), Some(REFILL_BATCH));
        // El anillo respeta la reserva del refill por demanda.
        assert_eq!(
            bank_batch_size(8000, 8192, cap - BANK_RING_HEADROOM, cap),
            None
        );
        assert_eq!(
            bank_batch_size(8000, 8192, cap - BANK_RING_HEADROOM - 10, cap),
            Some(10)
        );
        // El stock disponible acota el lote (buffer diminuto: marca = 6).
        assert_eq!(bank_batch_size(7, 8, 0, cap), Some(7));
    }

    /// KME falso que produce a `rate` claves/s de reloj real y sirve
    /// `/status` como un quditto en modo descarte: `stored` se recorta al
    /// techo (la producción que no cabe se pierde). El balance del estimador
    /// cierra por construcción, así que la medida debe clavar `rate` aunque
    /// el scheduler del test tenga jitter.
    struct RampKme {
        start: Instant,
        rate: f64,
        max: u64,
        delivered: AtomicU64,
        lost: AtomicU64,
    }

    impl RampKme {
        fn new(rate: f64, max: u64) -> Self {
            Self {
                start: Instant::now(),
                rate,
                max,
                delivered: AtomicU64::new(0),
                lost: AtomicU64::new(0),
            }
        }
        fn stored(&self) -> u64 {
            let produced = (self.start.elapsed().as_secs_f64() * self.rate) as u64;
            let gone = self.delivered.load(Ordering::Relaxed) + self.lost.load(Ordering::Relaxed);
            let raw = produced.saturating_sub(gone);
            if raw > self.max {
                // descarte al techo, como quditto: lo perdido no vuelve
                self.lost.fetch_add(raw - self.max, Ordering::Relaxed);
                self.max
            } else {
                raw
            }
        }
    }

    #[async_trait::async_trait]
    impl KeySource for RampKme {
        async fn enc_keys(&self, number: u32) -> crate::error::Result<Vec<OtpKey>> {
            let n = (u64::from(number)).min(self.stored());
            if n == 0 {
                return Err(crate::error::QkcError::NotEnoughKeys {
                    requested: number,
                    received: 0,
                });
            }
            self.delivered.fetch_add(n, Ordering::Relaxed);
            Ok((0..n)
                .map(|_| OtpKey {
                    key_id: Uuid::new_v4(),
                    material: Zeroizing::new(vec![0u8; 32]),
                })
                .collect())
        }
        async fn dec_keys(&self, _ids: &[Uuid]) -> crate::error::Result<Vec<OtpKey>> {
            Ok(Vec::new())
        }
        async fn stock(&self) -> crate::error::Result<Option<KmeStock>> {
            Ok(Some(KmeStock {
                stored: self.stored(),
                max: self.max,
            }))
        }
    }

    /// Integración del sondeador: sin NINGÚN consumidor, el estimador mide la
    /// producción (vía la subida del stock + el banking) y el banking mete en
    /// el anillo claves que el techo del KME habría destruido.
    #[tokio::test]
    async fn stock_poller_measures_and_banks_without_consumers() {
        let rate = 1000.0;
        let kme = Arc::new(RampKme::new(rate, 2000));
        let ks = KeyStore::new(
            kme,
            Arc::new(PeerOut::new()),
            2,
            "127.0.0.1:1".into(),
            1,
            256,
            None,
        );
        ks.spawn_workers();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let l = ks.levels();
            if let Some(est) = l.rate_est_kps {
                // Balance exacto por construcción del fake: 10 % de margen.
                if (est - rate).abs() < rate * 0.10 && l.enc_buffered > REFILL_THRESHOLD {
                    assert_eq!(l.rate_quality, Some("measured"));
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "sin converger: {:?} enc={}",
                l.rate_est_kps,
                l.enc_buffered
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Material QKD que el KME ya entregó y no cupo es material pagado que se
    /// pierde: "no debería pasar" no es razón para no contarlo.
    #[tokio::test]
    async fn keys_the_kme_already_delivered_are_counted_when_the_ring_is_full() {
        let ks = KeyStore::new(
            Arc::new(OverflowingKme),
            Arc::new(PeerOut::new()),
            2,
            "127.0.0.1:1".into(),
            1,
            256,
            None,
        );
        ks.spawn_workers();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let l = ks.levels();
            if l.enc_dropped_full > 0 {
                assert_eq!(l.enc_buffered, BUFFER_TARGET * 2, "el anillo quedó lleno");
                assert!(l.refills_enc >= 1);
                break;
            }
            assert!(Instant::now() < deadline, "el contador no subió: {l:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
