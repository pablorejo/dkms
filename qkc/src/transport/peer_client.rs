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
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use tokio::{net::TcpStream, sync::Notify};
use tracing::{debug, info, warn};
use wire::{write_frame, Frame};

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
    /// (drop count incrementa).
    pub fn send(&self, peer_id: u32, peer_addr: &str, frame: Frame) -> bool {
        let slot = self.get_or_create(peer_id, peer_addr);
        match slot.queue.push(frame) {
            Ok(()) => {
                slot.sent.fetch_add(1, Ordering::Relaxed);
                slot.notify.notify_one();
                true
            }
            Err(_) => {
                slot.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    fn get_or_create(&self, peer_id: u32, peer_addr: &str) -> Arc<PeerSlot> {
        if let Some(s) = self.peers.get(&peer_id) {
            return s.clone();
        }
        // Crea el slot + spawnea el writer task.
        let slot = Arc::new(PeerSlot {
            queue: Arc::new(ArrayQueue::new(QUEUE_CAPACITY)),
            notify: Arc::new(Notify::new()),
            addr: peer_addr.to_string(),
            sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
        });
        // Insertar es idempotente — si dos handlers entran a la vez,
        // un solo writer queda.
        let entry = self.peers.entry(peer_id).or_insert_with(|| slot.clone());
        let slot = entry.value().clone();
        drop(entry);
        // Spawn solo si lo hemos insertado en esta llamada (otherwise
        // ya hay un writer corriendo). Para detectarlo, comparamos el
        // Arc devuelto con el que construimos — son distintos si ya
        // existía.
        let writer_slot = slot.clone();
        let peer_id_for_log = peer_id;
        tokio::spawn(async move {
            writer_loop(peer_id_for_log, writer_slot).await;
        });
        slot
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
    loop {
        let mut stream = match TcpStream::connect(&slot.addr).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                info!(%peer_id, addr = %slot.addr, "qkc.peer_out.connected");
                backoff = Duration::from_millis(50);
                s
            }
            Err(e) => {
                debug!(%peer_id, addr = %slot.addr, error = %e, "qkc.peer_out.connect_failed");
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
                    slot.notify.notified().await;
                    continue;
                }
            };
            if let Err(e) = write_frame(&mut stream, &frame).await {
                warn!(%peer_id, error = %e, "qkc.peer_out.write_err");
                break; // reconectar
            }
        }
    }
}
