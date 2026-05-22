//! Canal de ACKs DKMS↔DKMS via TCP plano (sin ORR ni QKC).
//!
//! Protocolo: newline-delimited JSON. Cada línea es un mensaje:
//!
//! ```json
//! {"from":"dkms-22","key_ids":["uuid1","uuid2"]}
//! ```
//!
//! El servidor acepta conexiones de cualquier peer DKMS, lee líneas y
//! por cada `key_id` llama a [`Generator::on_ack`] para mover la entrada
//! correspondiente de `ack_pending` a `BufferPool.enc`.
//!
//! **Cliente — conexión persistente por peer.** La primera versión
//! abría un nuevo socket TCP por cada `send_batch`, asumiendo ACKs
//! sparse. Bajo carga de saturación esa asunción se rompe: la
//! campaña v13-validation midió 50-200 claves/s/DKMS expirando en
//! `ack_reaper` (timeout 30 s), pico de 4 042 claves en una sola
//! pasada en SECOQC. Ahora cada `peer_addr` mantiene un
//! `TcpStream` reutilizado entre llamadas, protegido por un
//! `tokio::sync::Mutex` que serializa las escrituras de líneas y se
//! reabre transparentemente al primer error de escritura.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex as TokioMutex;
use tracing::{debug, info, warn};

use common::ids::KeyId;

use crate::control::Generator;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckFrame {
    pub from: String,
    pub key_ids: Vec<String>,
}

/// Arranca el servidor TCP de ACKs. Cada conexión entrante se atiende en
/// su propio task; el handler lee líneas JSON, parsea `AckFrame`, y por
/// cada `key_id` llama a `gen.on_ack(from, key_id)`.
pub async fn serve(generator: Arc<Generator>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "dkms.ack_socket listening");
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "dkms.ack_socket accept failed");
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        debug!(%peer, "dkms.ack_socket accept");
        let gen = generator.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(gen, stream).await {
                debug!(error = %e, %peer, "dkms.ack_socket conn ended");
            }
        });
    }
}

async fn handle_conn(generator: Arc<Generator>, stream: TcpStream) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(()); // EOF
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame: AckFrame = match serde_json::from_str(trimmed) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, line = trimmed, "dkms.ack_socket bad frame");
                continue;
            }
        };
        let mut ok = 0;
        let mut miss = 0;
        for kid in &frame.key_ids {
            let key_id = KeyId::new(kid);
            if generator.on_ack(&frame.from, &key_id) {
                ok += 1;
            } else {
                miss += 1;
            }
        }
        debug!(from = %frame.from, ok, miss, "dkms.ack_socket batch processed");
    }
}

/// Cliente con conexión TCP persistente por peer. Cada `peer_addr` se
/// abre una sola vez la primera vez que se envía un batch; las
/// siguientes llamadas reutilizan el mismo `TcpStream`. Si la
/// escritura falla (broken pipe, reset, timeout) la entrada se
/// limpia y la próxima llamada abre una conexión fresca.
#[derive(Clone)]
pub struct AckClient {
    /// `my_dkms_id` — se incluye en cada `AckFrame.from` para que el peer
    /// sepa de quién viene el ACK.
    pub my_dkms_id: String,
    pub connect_timeout: Duration,
    pub write_timeout: Duration,
    /// `peer_addr` → conexión persistente. Se serializa con
    /// `tokio::sync::Mutex` para garantizar el orden de las líneas
    /// dentro del flujo TCP (newline-delimited framing).
    conns: Arc<DashMap<String, Arc<TokioMutex<Option<TcpStream>>>>>,
}

impl AckClient {
    pub fn new(my_dkms_id: String) -> Self {
        Self {
            my_dkms_id,
            connect_timeout: Duration::from_millis(2_000),
            write_timeout: Duration::from_millis(2_000),
            conns: Arc::new(DashMap::new()),
        }
    }

    /// Devuelve (o crea, lazy) el `Mutex` que guarda la conexión a
    /// `peer_addr`. No abre todavía la conexión TCP — eso lo hace
    /// `send_batch` la primera vez que necesita escribir.
    fn slot_for(&self, peer_addr: &str) -> Arc<TokioMutex<Option<TcpStream>>> {
        if let Some(s) = self.conns.get(peer_addr) {
            return s.clone();
        }
        // Slow path: insertar. `entry()` evita la doble lectura si
        // varios callers compiten por el mismo peer al mismo tiempo.
        let entry = self
            .conns
            .entry(peer_addr.to_string())
            .or_insert_with(|| Arc::new(TokioMutex::new(None)));
        entry.clone()
    }

    /// Envía un ACK con uno o más `key_ids` a la dirección `peer_addr`
    /// (host:port). Si la conexión persistente está sana se escribe la
    /// línea directamente; si está cerrada o falla la escritura se
    /// reabre una vez. Un único fallo de escritura tras reconectar se
    /// devuelve como error — el caller (BatchedAckClient) registra el
    /// warn y la próxima ronda lo reintentará.
    pub async fn send_batch(&self, peer_addr: &str, key_ids: &[String]) -> anyhow::Result<()> {
        if key_ids.is_empty() {
            return Ok(());
        }
        let frame = AckFrame {
            from: self.my_dkms_id.clone(),
            key_ids: key_ids.to_vec(),
        };
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');
        let bytes = line.into_bytes();

        let slot = self.slot_for(peer_addr);
        let mut guard = slot.lock().await;

        // Try the existing connection first. If it's missing or its
        // write fails, drop it and reconnect once. After two failed
        // attempts (no connection AND reconnect+write failed) we
        // surface an error.
        for attempt in 0..2 {
            if guard.is_none() {
                let stream =
                    tokio::time::timeout(self.connect_timeout, TcpStream::connect(peer_addr))
                        .await
                        .map_err(|_| {
                            anyhow::anyhow!("ack_socket connect timeout to {peer_addr}")
                        })??;
                let _ = stream.set_nodelay(true);
                *guard = Some(stream);
            }
            // SAFETY: the branch above ensures Some.
            let stream = guard.as_mut().expect("connection just opened");
            match tokio::time::timeout(self.write_timeout, stream.write_all(&bytes)).await {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(io_err)) => {
                    // Broken pipe / reset / etc. Drop the stream so
                    // the next iteration reconnects.
                    debug!(
                        peer_addr,
                        error = %io_err,
                        attempt,
                        "ack_socket persistent write failed, reconnecting"
                    );
                    *guard = None;
                }
                Err(_elapsed) => {
                    debug!(
                        peer_addr,
                        attempt, "ack_socket persistent write timed out, reconnecting"
                    );
                    *guard = None;
                }
            }
        }
        Err(anyhow::anyhow!(
            "ack_socket write to {peer_addr} failed after reconnect"
        ))
    }

    /// Conveniencia para un solo `key_id`.
    pub async fn send_one(&self, peer_addr: &str, key_id: &str) -> anyhow::Result<()> {
        self.send_batch(peer_addr, &[key_id.to_string()]).await
    }
}

/// Agrupa ACKs en cola y los flushea cada `interval` o cuando se acumulan
/// `max_keys` para un mismo peer_addr. Reduce el coste de TCP open/close
/// bajo carga sostenida.
#[derive(Clone)]
pub struct BatchedAckClient {
    inner: Arc<AckClient>,
    pending: Arc<parking_lot::Mutex<HashMap<String, Vec<String>>>>,
    max_keys: usize,
}

impl BatchedAckClient {
    pub fn new(client: AckClient, max_keys: usize, flush_interval: Duration) -> Self {
        let b = Self {
            inner: Arc::new(client),
            pending: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            max_keys,
        };
        let me = b.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(flush_interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                me.flush_all().await;
            }
        });
        b
    }

    /// Encola un ACK. Si la cola para ese peer_addr llega al cap, hace
    /// flush inmediato.
    pub async fn enqueue(&self, peer_addr: String, key_id: String) {
        let flush_now: Option<Vec<String>> = {
            let mut g = self.pending.lock();
            let bucket = g.entry(peer_addr.clone()).or_default();
            bucket.push(key_id);
            if bucket.len() >= self.max_keys {
                Some(std::mem::take(bucket))
            } else {
                None
            }
        };
        if let Some(batch) = flush_now {
            if let Err(e) = self.inner.send_batch(&peer_addr, &batch).await {
                warn!(peer_addr = %peer_addr, error = %e, "ack_socket send_batch (full) failed");
            }
        }
    }

    async fn flush_all(&self) {
        let snapshot: Vec<(String, Vec<String>)> = {
            let mut g = self.pending.lock();
            let drained: Vec<(String, Vec<String>)> = g
                .iter_mut()
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.clone(), std::mem::take(v)))
                .collect();
            drained
        };
        // Fire all peers in parallel — with persistent per-peer
        // connections (see `AckClient::send_batch`) the writes are
        // independent, so serial dispatch was just adding the
        // network RTT of each peer to the next peer's deadline. At
        // ~20 peers and 50 ms flush interval that gap was enough to
        // miss the next tick and grow the queue.
        let inner = self.inner.clone();
        let handles: Vec<_> = snapshot
            .into_iter()
            .map(|(addr, batch)| {
                let inner = inner.clone();
                tokio::spawn(async move {
                    if let Err(e) = inner.send_batch(&addr, &batch).await {
                        warn!(
                            peer_addr = %addr,
                            error = %e,
                            "ack_socket send_batch (tick) failed"
                        );
                    }
                })
            })
            .collect();
        for h in handles {
            let _ = h.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[test]
    fn ack_frame_roundtrip() {
        let f = AckFrame {
            from: "dkms-11".into(),
            key_ids: vec!["a".into(), "b".into()],
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: AckFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.from, "dkms-11");
        assert_eq!(back.key_ids, vec!["a".to_string(), "b".to_string()]);
    }

    /// Spin up a server that accepts ONE connection, counts how many
    /// newline frames arrive on it, and exposes the count. Lets a
    /// test prove the AckClient reuses a single connection across
    /// `send_batch` calls instead of opening a fresh socket per call.
    struct CountingServer {
        addr: std::net::SocketAddr,
        accepts: Arc<AtomicUsize>,
        frames: Arc<AtomicUsize>,
    }

    async fn spawn_counting_server() -> CountingServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(AtomicUsize::new(0));
        let accepts_c = accepts.clone();
        let frames_c = frames.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                accepts_c.fetch_add(1, Ordering::SeqCst);
                let frames_c = frames_c.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) => return,
                            Ok(_) => {
                                frames_c.fetch_add(1, Ordering::SeqCst);
                            }
                            Err(_) => return,
                        }
                    }
                });
            }
        });
        CountingServer {
            addr,
            accepts,
            frames,
        }
    }

    #[tokio::test]
    async fn persistent_connection_is_reused_across_sends() {
        // Four sequential send_batch calls to the same peer should
        // produce ONE TCP accept on the server side (because we now
        // hold a persistent connection per peer) and FOUR frames.
        // Before this refactor each call did its own connect/shutdown
        // so accepts would have been 4 too.
        let server = spawn_counting_server().await;
        let client = AckClient::new("dkms-X".into());
        let peer = server.addr.to_string();
        for i in 0..4 {
            client
                .send_batch(&peer, &[format!("kid-{i}")])
                .await
                .unwrap();
        }
        // Let the server drain its read loop.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            server.accepts.load(Ordering::SeqCst),
            1,
            "expected exactly one TCP accept, got {}",
            server.accepts.load(Ordering::SeqCst)
        );
        assert_eq!(server.frames.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn dead_connection_triggers_reconnect() {
        // Sim: the server closes each TCP after the first frame
        // (server restart / idle TCP timeout). The client must
        // eventually reopen the connection and keep delivering
        // frames. Linux TCP semantics mean the first post-close
        // write may quietly succeed into the send buffer; the next
        // one gets EPIPE and triggers our reconnect. So we send
        // several frames and assert at least two accepts and the
        // last frame was definitely received.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let frames = Arc::new(AtomicUsize::new(0));
        let accepts_c = accepts.clone();
        let frames_c = frames.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                accepts_c.fetch_add(1, Ordering::SeqCst);
                let frames_c = frames_c.clone();
                tokio::spawn(async move {
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    // Read EXACTLY one line, then drop → server FIN.
                    if let Ok(n) = reader.read_line(&mut line).await {
                        if n > 0 {
                            frames_c.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                });
            }
        });
        let client = AckClient::new("dkms-X".into());
        let peer = addr.to_string();
        for i in 0..6 {
            // Some sends may fail (the very first write after the
            // peer FIN occasionally goes through silently, the next
            // one returns EPIPE). The contract is "eventually
            // delivered", not "every call succeeds against a
            // self-destructing server".
            let _ = client.send_batch(&peer, &[format!("k{i}")]).await;
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            accepts.load(Ordering::SeqCst) >= 2,
            "expected >= 2 reconnects after server closes, got accepts={} frames={}",
            accepts.load(Ordering::SeqCst),
            frames.load(Ordering::SeqCst)
        );
        assert!(frames.load(Ordering::SeqCst) >= 2);
    }
}
