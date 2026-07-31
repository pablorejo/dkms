//! Pool de salida QKC→QKC.
//!
//! Una **conexión TCP persistente** por peer + una **cola lock-free**
//! (`crossbeam::ArrayQueue`) que el writer task drena. El handler que
//! genera frames hace `enqueue(frame)` y vuelve inmediatamente —
//! fire-and-forget puro. Si la cola está llena se cuenta un drop.
//!
//! Si la conexión cae, el writer task reabre con backoff exponencial
//! corto. No se serializa el envío detrás de la reconexión: los frames
//! que lleguen mientras se reconecta se dropean (la fiabilidad
//! end-to-end la cubre la capa de aplicación, no esta).

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use tokio::{net::TcpStream, sync::Notify};
use tracing::{info, warn};
use wire::{write_frame, Frame};

/// Periodo entre logs INFO de stats periódicas del peer_out. Cada
/// 30 s emite por peer: queue_depth, sent/dropped totales,
/// sent_kps/drop_kps en la ventana, connected_state. Útil para
/// diagnosticar stalls del link QKC↔QKC cuando los frames
/// no llegan al destino (smoke 2026-05-25: 16 min de stall en
/// silencio antes de añadir esto).
const STATS_LOG_PERIOD: Duration = Duration::from_secs(30);

/// Throttle para warn de drops en `send()`: como mucho 1 cada esta
/// ventana por peer. Sin esto, un peer cuya cola se llena emitiría
/// miles de warn/s y los logs se ahogarían.
const DROP_WARN_THROTTLE: Duration = Duration::from_secs(1);

/// Throttle equivalente para warn de connect_failed. Bajo backoff de
/// 2 s entre intentos, eso es ~30 attempts/min. Sin throttle emitiría
/// 1 WARN cada 2 s — muy útil para diagnóstico pero ruidoso. Logueamos
/// el PRIMER fallo siempre y luego como máximo 1 cada 10 s.
const CONNECT_FAILED_WARN_THROTTLE: Duration = Duration::from_secs(10);

/// Capacidad de la cola por peer. Bajo ráfagas grandes (hub recibe
/// frames de las 4 ramas en paralelo y re-encripta hacia cada destino)
/// la cola puede crecer rápido; 32k da margen para ~30k frames
/// sin tener que dropear.
const QUEUE_CAPACITY: usize = 32_768;

struct PeerSlot {
    queue: Arc<ArrayQueue<Frame>>,
    notify: Arc<Notify>,
    addr: String,
    sent: AtomicU64,
    dropped: AtomicU64,
    /// Último Instant en que emitimos WARN de drop, en micros desde
    /// UNIX_EPOCH (cabe en u64). Usado para throttle de drop warns.
    last_drop_warn_micros: AtomicU64,
    /// `true` mientras el writer_loop tiene un TCP stream abierto;
    /// `false` si está en backoff de reconexión. Útil para stats.
    connected: AtomicBool,
    /// Conexiones TCP logradas contra este peer desde el arranque.
    connects: AtomicU64,
    /// Se avisa en cada RE-conexión (la primera no cuenta). Que el socket
    /// se caiga y vuelva es la única señal local de que el peer pudo
    /// haberse reiniciado y perdido su estado — la usa el handshake PQC
    /// para renegociar el enlace. Ver `PqcHandshake::spawn_rotation`.
    reconnect: Arc<Notify>,
}

pub struct PeerOut {
    peers: DashMap<u32, Arc<PeerSlot>>,
}

impl PeerOut {
    pub fn new() -> Self {
        Self {
            peers: DashMap::new(),
        }
    }

    /// Encola un frame para `peer_id`. `false` si la cola está llena
    /// (drop count incrementa). Emite WARN throttled cada
    /// [`DROP_WARN_THROTTLE`] para no spammear bajo congestión.
    pub fn send(&self, peer_id: u32, peer_addr: &str, frame: Frame) -> bool {
        let slot = self.get_or_create(peer_id, peer_addr);
        match slot.queue.push(frame) {
            Ok(()) => {
                slot.sent.fetch_add(1, Ordering::Relaxed);
                slot.notify.notify_one();
                true
            }
            Err(_) => {
                let dropped_now = slot.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                // Throttle de WARN: emitimos si pasaron > DROP_WARN_THROTTLE
                // desde el último warn. Compare-and-swap evita doble warn
                // si dos hilos chocan exactamente al filo.
                let now_us = micros_since_epoch();
                let last_us = slot.last_drop_warn_micros.load(Ordering::Relaxed);
                let threshold_us = DROP_WARN_THROTTLE.as_micros() as u64;
                if now_us.saturating_sub(last_us) >= threshold_us
                    && slot
                        .last_drop_warn_micros
                        .compare_exchange(last_us, now_us, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                {
                    warn!(
                        %peer_id,
                        addr = %slot.addr,
                        queue_capacity = QUEUE_CAPACITY,
                        dropped_total = dropped_now,
                        connected = slot.connected.load(Ordering::Relaxed),
                        "qkc.peer_out.drop (queue full, frame discarded)"
                    );
                }
                false
            }
        }
    }

    fn get_or_create(&self, peer_id: u32, peer_addr: &str) -> Arc<PeerSlot> {
        if let Some(s) = self.peers.get(&peer_id) {
            return s.clone();
        }
        let candidate = Arc::new(PeerSlot {
            queue: Arc::new(ArrayQueue::new(QUEUE_CAPACITY)),
            notify: Arc::new(Notify::new()),
            addr: peer_addr.to_string(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            last_drop_warn_micros: AtomicU64::new(0),
            connected: AtomicBool::new(false),
            connects: AtomicU64::new(0),
            reconnect: Arc::new(Notify::new()),
        });
        let entry = self
            .peers
            .entry(peer_id)
            .or_insert_with(|| candidate.clone());
        let stored = entry.value().clone();
        drop(entry);
        // Solo arrancar el writer si nuestro candidato fue el que quedó
        // guardado en el DashMap. Si otro hilo (o una llamada previa)
        // ya tenía un slot dentro, `or_insert_with` lo devuelve y
        // descarta `candidate`; spawnear aquí en ese caso fugaba una
        // task tokio por cada send() y, en hubs de topologías densas,
        // saturaba la memoria del sidecar (OOM ~3 min en BA hub con 12
        // peers, observado en sim 16/17/18 / v14-validation).
        if Arc::ptr_eq(&stored, &candidate) {
            let writer_slot = stored.clone();
            let stats_slot = stored.clone();
            let peer_id_for_log = peer_id;
            tokio::spawn(async move {
                writer_loop(peer_id_for_log, writer_slot).await;
            });
            // Stats logger en background: emite INFO cada
            // STATS_LOG_PERIOD con queue depth + sent/dropped totales
            // y tasas instantáneas. Sobrevive a reconexiones porque
            // observa el PeerSlot, no el stream TCP.
            tokio::spawn(async move {
                stats_logger(peer_id_for_log, stats_slot).await;
            });
        }
        stored
    }

    /// Handle que se despierta cada vez que RE-conectamos con `peer_id`
    /// (la primera conexión no dispara). Crea el slot si hace falta, así
    /// que se puede pedir antes del primer `send`.
    pub fn reconnect_signal(&self, peer_id: u32, peer_addr: &str) -> Arc<Notify> {
        self.get_or_create(peer_id, peer_addr).reconnect.clone()
    }

    pub fn stats(&self, peer_id: u32) -> Option<(u64, u64)> {
        self.peers.get(&peer_id).map(|s| {
            (
                s.sent.load(Ordering::Relaxed),
                s.dropped.load(Ordering::Relaxed),
            )
        })
    }
}

impl Default for PeerOut {
    fn default() -> Self {
        Self::new()
    }
}

async fn writer_loop(peer_id: u32, slot: Arc<PeerSlot>) {
    let mut backoff = Duration::from_millis(50);
    let mut last_connect_warn = Instant::now() - CONNECT_FAILED_WARN_THROTTLE;
    let mut consecutive_failures: u64 = 0;
    loop {
        let mut stream = match TcpStream::connect(&slot.addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                if consecutive_failures > 0 {
                    info!(
                        %peer_id,
                        addr = %slot.addr,
                        recovered_after_failures = consecutive_failures,
                        "qkc.peer_out.connected (recovered)"
                    );
                } else {
                    info!(%peer_id, addr = %slot.addr, "qkc.peer_out.connected");
                }
                slot.connected.store(true, Ordering::Relaxed);
                // La primera conexión es el arranque normal; a partir de la
                // segunda, el peer pudo haberse reiniciado. `notify_one`
                // guarda el permiso, así que el aviso no se pierde aunque
                // nadie esté esperando en este instante.
                if slot.connects.fetch_add(1, Ordering::Relaxed) > 0 {
                    slot.reconnect.notify_one();
                }
                backoff = Duration::from_millis(50);
                consecutive_failures = 0;
                s
            }
            Err(e) => {
                consecutive_failures += 1;
                // Promovido de debug → warn con throttle. El primer
                // fallo siempre se loguea. Los siguientes solo cada
                // CONNECT_FAILED_WARN_THROTTLE. Esto da visibilidad
                // de stalls de conexión sin spam bajo backoff intenso.
                if consecutive_failures == 1
                    || last_connect_warn.elapsed() >= CONNECT_FAILED_WARN_THROTTLE
                {
                    warn!(
                        %peer_id,
                        addr = %slot.addr,
                        error = %e,
                        consecutive_failures,
                        backoff_ms = backoff.as_millis() as u64,
                        "qkc.peer_out.connect_failed"
                    );
                    last_connect_warn = Instant::now();
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(2));
                continue;
            }
        };

        // Drena la cola mientras el socket esté vivo.
        loop {
            // Pop hasta vaciar, o esperar notify si vacía.
            let frame = match slot.queue.pop() {
                Some(f) => f,
                None => {
                    // Con la cola vacía esperamos a que llegue algo que
                    // enviar O a que el socket se muera. Esto último
                    // importa: este canal es de una sola dirección (el peer
                    // nos escribe por SU propia conexión, ver
                    // `peer_server`), así que que se vuelva legible sólo
                    // puede ser EOF. Sin esta rama, un enlace ocioso no se
                    // enteraba de que el peer se había reiniciado hasta el
                    // siguiente envío — y si el motivo de no enviar era
                    // precisamente que el peer estaba caído, no había
                    // siguiente envío. El handshake PQC se apoya en esta
                    // detección para renegociar el enlace.
                    tokio::select! {
                        _ = slot.notify.notified() => continue,
                        r = stream.readable() => {
                            if peer_hung_up(peer_id, &stream, r) {
                                slot.connected.store(false, Ordering::Relaxed);
                                break;
                            }
                            continue;
                        }
                    }
                }
            };
            if let Err(e) = write_frame(&mut stream, &frame).await {
                slot.connected.store(false, Ordering::Relaxed);
                warn!(%peer_id, addr = %slot.addr, error = %e, "qkc.peer_out.write_err");
                break; // reconectar
            }
        }
    }
}

/// Background task: cada STATS_LOG_PERIOD emite un INFO con stats
/// del peer. Diferencia respecto al snapshot anterior para mostrar
/// tasas instantáneas. Permite observar stalls del link incluso
/// cuando el writer no emite write_err (e.g. TCP half-open zombie).
async fn stats_logger(peer_id: u32, slot: Arc<PeerSlot>) {
    let mut prev_sent = slot.sent.load(Ordering::Relaxed);
    let mut prev_dropped = slot.dropped.load(Ordering::Relaxed);
    let mut tick = tokio::time::interval(STATS_LOG_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // saltar el primer tick instantáneo
    loop {
        tick.tick().await;
        let sent = slot.sent.load(Ordering::Relaxed);
        let dropped = slot.dropped.load(Ordering::Relaxed);
        let dt = STATS_LOG_PERIOD.as_secs_f64();
        let sent_rate = (sent.saturating_sub(prev_sent)) as f64 / dt;
        let drop_rate = (dropped.saturating_sub(prev_dropped)) as f64 / dt;
        info!(
            %peer_id,
            addr = %slot.addr,
            connected = slot.connected.load(Ordering::Relaxed),
            queue_depth = slot.queue.len(),
            queue_cap = QUEUE_CAPACITY,
            sent_total = sent,
            dropped_total = dropped,
            sent_kps = format!("{sent_rate:.1}"),
            drop_kps = format!("{drop_rate:.1}"),
            "qkc.peer_out.stats",
        );
        prev_sent = sent;
        prev_dropped = dropped;
    }
}

/// ¿El socket de salida está muerto? Se llama cuando `readable()` ha
/// resuelto. Como el peer nunca escribe por aquí, legible = EOF o error;
/// datos inesperados se ignoran (no rompemos el enlace por eso).
fn peer_hung_up(peer_id: u32, stream: &TcpStream, readable: std::io::Result<()>) -> bool {
    if let Err(e) = readable {
        warn!(%peer_id, error = %e, "qkc.peer_out.idle_poll_err");
        return true;
    }
    let mut scratch = [0u8; 64];
    match stream.try_read(&mut scratch) {
        Ok(0) => {
            info!(%peer_id, "qkc.peer_out.peer_hung_up (EOF con la cola vacía)");
            true
        }
        Ok(n) => {
            warn!(%peer_id, bytes = n, "qkc.peer_out: bytes inesperados en el canal de salida");
            false
        }
        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(e) => {
            warn!(%peer_id, error = %e, "qkc.peer_out.idle_read_err");
            true
        }
    }
}

#[inline]
fn micros_since_epoch() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// La señal de reconexión es lo que resucita un enlace PQC cuyo
    /// respondedor se reinició, así que tiene que distinguir la primera
    /// conexión (arranque normal) de las siguientes (el peer se fue).
    #[tokio::test]
    async fn reconnect_fires_on_the_second_connect_but_not_the_first() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let out = PeerOut::new();
        let signal = out.reconnect_signal(7, &addr);

        // Primera conexión: el writer_loop se lanzó al pedir la señal.
        let (first, _) = listener.accept().await.unwrap();
        // Nadie debe haber avisado todavía.
        let too_soon = tokio::time::timeout(Duration::from_millis(200), signal.notified()).await;
        assert!(
            too_soon.is_err(),
            "la primera conexión no es una reconexión"
        );

        // El peer "se reinicia": cae el socket. Sin enviar NADA — es el
        // caso que importa, porque un enlace ocioso que no se entera de
        // que el peer se fue no renegocia nunca.
        drop(first);
        let reconnected = tokio::time::timeout(Duration::from_secs(10), listener.accept()).await;
        assert!(
            reconnected.is_ok(),
            "el writer_loop detecta el EOF con la cola vacía y reconecta",
        );

        let fired = tokio::time::timeout(Duration::from_secs(5), signal.notified()).await;
        assert!(fired.is_ok(), "la reconexión sí avisa");
    }
}
