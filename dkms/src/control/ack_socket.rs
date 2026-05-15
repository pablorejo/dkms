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
//! El cliente abre una nueva conexión TCP por cada ACK (no pooling). Las
//! ACKs son sparse (1 por clave generada y recibida) así que el coste
//! del handshake TCP por ACK es despreciable frente al ahorro de
//! complejidad. Si en producción esto se vuelve un cuello, añadiremos
//! pooling o un cliente persistente per-peer.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, info, warn};

use common::ids::KeyId;

use crate::control::Generator;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckFrame {
    pub from:    String,
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

/// Cliente que envía ACKs en batch a un peer. Sin estado entre llamadas:
/// abre nueva conexión TCP, escribe una línea, cierra.
#[derive(Clone)]
pub struct AckClient {
    /// `my_dkms_id` — se incluye en cada `AckFrame.from` para que el peer
    /// sepa de quién viene el ACK.
    pub my_dkms_id: String,
    pub connect_timeout: Duration,
    pub write_timeout: Duration,
}

impl AckClient {
    pub fn new(my_dkms_id: String) -> Self {
        Self {
            my_dkms_id,
            connect_timeout: Duration::from_millis(2_000),
            write_timeout: Duration::from_millis(2_000),
        }
    }

    /// Envía un ACK con uno o más `key_ids` a la dirección `peer_addr`
    /// (host:port). No espera respuesta. Si la conexión falla, el peer
    /// tendrá que esperar a que las entradas expiren por timeout (lo
    /// cual la siguiente ronda de generación absorberá).
    pub async fn send_batch(&self, peer_addr: &str, key_ids: &[String]) -> anyhow::Result<()> {
        if key_ids.is_empty() {
            return Ok(());
        }
        let frame = AckFrame {
            from:    self.my_dkms_id.clone(),
            key_ids: key_ids.iter().cloned().collect(),
        };
        let mut line = serde_json::to_string(&frame)?;
        line.push('\n');

        let stream = tokio::time::timeout(self.connect_timeout, TcpStream::connect(peer_addr))
            .await
            .map_err(|_| anyhow::anyhow!("ack_socket connect timeout to {peer_addr}"))??;
        let _ = stream.set_nodelay(true);
        let mut stream = stream;
        tokio::time::timeout(self.write_timeout, stream.write_all(line.as_bytes()))
            .await
            .map_err(|_| anyhow::anyhow!("ack_socket write timeout to {peer_addr}"))??;
        let _ = stream.shutdown().await;
        Ok(())
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
    inner:    Arc<AckClient>,
    pending:  Arc<parking_lot::Mutex<HashMap<String, Vec<String>>>>,
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
        for (addr, batch) in snapshot {
            if let Err(e) = self.inner.send_batch(&addr, &batch).await {
                warn!(peer_addr = %addr, error = %e, "ack_socket send_batch (tick) failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_frame_roundtrip() {
        let f = AckFrame {
            from:    "dkms-11".into(),
            key_ids: vec!["a".into(), "b".into()],
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: AckFrame = serde_json::from_str(&s).unwrap();
        assert_eq!(back.from, "dkms-11");
        assert_eq!(back.key_ids, vec!["a".to_string(), "b".to_string()]);
    }
}
