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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use common::ids::KeyId;

use crate::control::{flow_stats::FlowStats, Generator};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckFrame {
    pub from: String,
    pub key_ids: Vec<String>,
}

/// Tope de una línea de ACK (B3): sin él, `read_line` crecía un `String` sin
/// límite pre-auth. Un batch de ~1500 uuids son ~55 KB; 64 KiB va holgado.
const MAX_ACK_LINE: u64 = 64 * 1024;
/// Tope de longitud del campo `from` de un ACK (B5): un `from` de varios MB
/// multiplicaba el tamaño de cada línea de log.
const MAX_ACK_FROM: usize = 128;

/// ACKs con `from` desconocido o absurdo, descartados sin crear estado por-peer.
/// Global (una sola entrada), no por `from` — ahí estaba la fuga (B5).
static UNKNOWN_ACK_FROM: AtomicU64 = AtomicU64::new(0);
static BAD_ACK_FRAMES: AtomicU64 = AtomicU64::new(0);

/// ¿Se procesa este ACK? `from` acotado y perteneciente al registro de peers.
/// Aislado para poder testearlo sin un `Generator`.
fn ack_from_is_processable(from: &str, is_known_peer: bool) -> bool {
    from.len() <= MAX_ACK_FROM && is_known_peer
}
/// Conexiones concurrentes al socket de ACK (B3): sin cota, N sockets ociosos
/// vivían para siempre.
const MAX_ACK_CONNS: usize = 128;
/// Una conexión sin actividad más de esto se cierra (B3).
const ACK_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Lee una línea acotada a `max` bytes. `Ok(Some(0))` = EOF; `Ok(None)` = la
/// línea superó el tope sin cerrar (conexión abusiva → el llamador cierra).
/// Sustituye a `read_line`, que no acota (B3).
async fn read_bounded_line<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut String,
    max: u64,
) -> std::io::Result<Option<usize>> {
    buf.clear();
    let n = (&mut *reader).take(max).read_line(buf).await?;
    if n == 0 {
        return Ok(Some(0));
    }
    if !buf.ends_with('\n') {
        return Ok(None);
    }
    Ok(Some(n))
}

/// Arranca el servidor TCP de ACKs. Cada conexión entrante se atiende en
/// su propio task; el handler lee líneas JSON, parsea `AckFrame`, y por
/// cada `key_id` llama a `gen.on_ack(from, key_id)`.
pub async fn serve(generator: Arc<Generator>, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let listener = common::net::bind_reuse_addr(addr).await?;
    info!(%addr, "dkms.ack_socket listening");
    // Direcciones remotas ya vistas: la primera conexión de cada peer se
    // loguea a INFO. Sin esto, "no llega ni un ACK" y "llegan pero no
    // casan" son indistinguibles sin activar debug.
    let seen: Arc<parking_lot::Mutex<std::collections::HashSet<std::net::IpAddr>>> =
        Arc::new(parking_lot::Mutex::new(std::collections::HashSet::new()));
    let conns = Arc::new(Semaphore::new(MAX_ACK_CONNS));
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "dkms.ack_socket accept failed");
                continue;
            }
        };
        // Cota de conexiones concurrentes (B3): a tope, cerramos la nueva en
        // vez de encolar una task por socket ocioso.
        let Ok(permit) = conns.clone().try_acquire_owned() else {
            debug!(%peer, "dkms.ack_socket: tope de conexiones, rechazo");
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        {
            // Acotado (B-10): con IPv6 las direcciones son gratis.
            let mut s = seen.lock();
            if s.len() < 1024 && s.insert(peer.ip()) {
                info!(from_ip = %peer.ip(), "dkms.ack_socket: primera conexión de ACK desde esta IP");
            }
        }
        debug!(%peer, "dkms.ack_socket accept");
        let gen = generator.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_conn(gen, stream, peer).await {
                debug!(error = %e, %peer, "dkms.ack_socket conn ended");
            }
        });
    }
}

async fn handle_conn(
    generator: Arc<Generator>,
    stream: TcpStream,
    remote: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        // Lectura acotada + timeout de inactividad (B3).
        let n = match tokio::time::timeout(
            ACK_IDLE_TIMEOUT,
            read_bounded_line(&mut reader, &mut line, MAX_ACK_LINE),
        )
        .await
        {
            Err(_) => {
                debug!(%remote, "dkms.ack_socket: conexión ociosa, cierro");
                return Ok(());
            }
            Ok(Err(e)) => return Err(e.into()),
            Ok(Ok(None)) => {
                warn!(%remote, "dkms.ack_socket: línea > tope, cierro la conexión");
                return Ok(());
            }
            Ok(Ok(Some(0))) => return Ok(()), // EOF
            Ok(Ok(Some(n))) => n,
        };
        let _ = n;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let frame: AckFrame = match serde_json::from_str(trimmed) {
            Ok(f) => f,
            Err(e) => {
                // Escapado y truncado (B-10): bytes elegidos por quien conecta.
                let shown: String = trimmed.chars().take(200).collect();
                let n = BAD_ACK_FRAMES.fetch_add(1, Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n) {
                    warn!(error = %e, line = ?shown, %remote, bad = n + 1, "dkms.ack_socket bad frame");
                }
                continue;
            }
        };
        // Descartar `from` desconocido/absurdo ANTES de tocar estado (B5): sin
        // esto, cada `from` inventado creaba un PeerFlow permanente y una línea
        // de log cada 5 s (la clase "752 MB/10 min"). Contador global.
        if !ack_from_is_processable(&frame.from, generator.is_known_ack_peer(&frame.from)) {
            let n = UNKNOWN_ACK_FROM.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                warn!(from = %frame.from, %remote, total = n,
                    "dkms.ack_socket: ACK de un `from` desconocido, descartado (B5)");
            }
            continue;
        }
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
        // Un batch entero fallido es la firma de un desajuste de identidad
        // o de un `ack_timeout_ms` demasiado corto; `on_ack` ya distingue
        // cuál de los dos y lo loguea con rate-limit.
        if ok == 0 && miss > 0 {
            debug!(from = %frame.from, %remote, miss, "dkms.ack_socket batch entero sin casar");
        }
        debug!(from = %frame.from, %remote, ok, miss, "dkms.ack_socket batch processed");
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
            from: self.my_dkms_id.clone(),
            key_ids: key_ids.to_vec(),
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
/// Transporte ETSI-020 para los ACK salientes: la variante autenticada.
///
/// El socket TCP heredado acepta de cualquiera y se cree el `from` del cuerpo,
/// así que un ACK forjado saca entradas de `ack_pending` y descuadra el
/// generador. Por mTLS la identidad la pone el certificado de cliente, y el
/// receptor (`handle_ext_keys_ack` → `handle_incoming_ack`) ya la usaba: lo que
/// faltaba era este lado.
///
/// No se enruta por el ORR a propósito: eso metería los ACK dentro del OTP del
/// enlace, que trocea en bloques de 32 B y gasta una clave QKD por bloque — un
/// lote de 32 `key_id` son ~38 claves, y a 13 000 claves/s eso multiplica el
/// consumo. mTLS no cuesta material.
#[derive(Clone)]
pub struct Etsi020AckTransport {
    pub client: Arc<crate::peer_client::PeerHttpClient>,
    pub peers: Arc<crate::peers::PeerRegistry>,
}

#[derive(Clone)]
pub struct BatchedAckClient {
    inner: Arc<AckClient>,
    /// Si está, los ACK salen por aquí y el socket queda de respaldo para los
    /// peers que no tengan endpoint HTTP.
    etsi020: Option<Etsi020AckTransport>,
    /// `peer_dkms_id → (ack_endpoint, key_ids en cola)`. Indexado por peer
    /// y no por dirección para que los contadores de
    /// [`crate::control::flow_stats`] hablen el mismo idioma que el resto
    /// del camino de claves.
    pending: Arc<parking_lot::Mutex<HashMap<String, (String, Vec<String>)>>>,
    stats: Arc<FlowStats>,
    max_keys: usize,
}

impl BatchedAckClient {
    pub fn new(
        client: AckClient,
        max_keys: usize,
        flush_interval: Duration,
        stats: Arc<FlowStats>,
    ) -> Self {
        Self::with_transport(client, max_keys, flush_interval, stats, None)
    }

    pub fn with_transport(
        client: AckClient,
        max_keys: usize,
        flush_interval: Duration,
        stats: Arc<FlowStats>,
        etsi020: Option<Etsi020AckTransport>,
    ) -> Self {
        let b = Self {
            inner: Arc::new(client),
            etsi020,
            pending: Arc::new(parking_lot::Mutex::new(HashMap::new())),
            stats,
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

    /// Encola un ACK hacia `peer_id` en la dirección que él anunció. Si la
    /// cola llega al cap, hace flush inmediato.
    pub async fn enqueue(&self, peer_id: &str, peer_addr: String, key_id: String) {
        self.stats.ack_enqueued(peer_id, 1);
        let flush_now: Option<Vec<String>> = {
            let mut g = self.pending.lock();
            let bucket = g
                .entry(peer_id.to_owned())
                .or_insert_with(|| (peer_addr.clone(), Vec::new()));
            // El peer puede haber cambiado de dirección (redespliegue con
            // otra IP): mandamos siempre a la última anunciada.
            bucket.0 = peer_addr.clone();
            bucket.1.push(key_id);
            if bucket.1.len() >= self.max_keys {
                Some(std::mem::take(&mut bucket.1))
            } else {
                None
            }
        };
        if let Some(batch) = flush_now {
            // 2026-05-24 fix #2: spawn en vez de await para no bloquear el
            // ORR delivery pump (single-threaded) mientras se hace el TCP
            // connect+write+shutdown del send_batch. Antes, cada bucket que
            // se llenaba (max_keys=32) congelaba el pump → keys entrantes
            // se acumulaban en el broadcast channel → Lagged → expired.
            let me = self.clone();
            let peer = peer_id.to_owned();
            tokio::spawn(async move {
                me.send_and_count(&peer, &peer_addr, batch, "full").await;
            });
        }
    }

    /// ¿Hay camino ETSI-020 hacia este peer? (transporte etsi020 configurado
    /// Y endpoint HTTP conocido). Es lo que permite acusar recibo aunque el
    /// emisor no anuncie `ack_endpoint` — con `ack_socket_listen = false` ya
    /// no lo anuncia, y ese header es SOLO del socket heredado.
    pub fn has_etsi020_route(&self, peer_id: &str) -> bool {
        self.etsi020.as_ref().is_some_and(|t| {
            t.peers
                .get(peer_id)
                .is_some_and(|cfg| !cfg.endpoint.is_empty())
        })
    }

    /// Manda un batch y contabiliza el resultado. `why` distingue el flush
    /// por cola llena del periódico, para leer en el log si el ritmo de
    /// ACK lo marca la carga o el temporizador.
    async fn send_and_count(&self, peer_id: &str, addr: &str, batch: Vec<String>, why: &str) {
        let n = batch.len() as u64;
        // Camino autenticado si está configurado Y el peer tiene endpoint HTTP.
        // Si no, el socket: un despliegue ORR-only no tiene `peer_client`, y
        // dejar el ACK sin mandar sería peor que mandarlo sin autenticar.
        if let Some(t) = &self.etsi020 {
            if let Some(cfg) = t.peers.get(peer_id) {
                if !cfg.endpoint.is_empty() {
                    match t.client.send_ext_keys_ack(peer_id, &cfg, &batch).await {
                        Ok(matched) => {
                            self.stats
                                .peer(peer_id)
                                .ack_sent
                                .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                            debug!(peer = peer_id, n, matched, why, "ack etsi020 enviado");
                            return;
                        }
                        Err(e) => {
                            // No se cae al socket: si el operador pidió ACK
                            // autenticado, mandarlo en claro por detrás
                            // anularía en silencio lo que pidió. Un lote
                            // cada 50 ms por peer mientras dure el fallo:
                            // el contador va a `generator.state`, el log
                            // habla en las potencias de dos.
                            self.stats.ack_send_failed(peer_id, n);
                            let total = self
                                .stats
                                .peer(peer_id)
                                .ack_send_failed
                                .load(std::sync::atomic::Ordering::Relaxed);
                            if common::log_throttle::nth_is_loud(total.saturating_sub(n)) {
                                warn!(peer = peer_id, error = %e, n, why, failed_total = total,
                                      "ack etsi020 falló; NO caigo al socket sin autenticar");
                            }
                            return;
                        }
                    }
                }
            }
        }
        if addr.is_empty() {
            // Encolado sin endpoint de socket (el emisor no lo anuncia) y el
            // camino etsi020 de arriba no aplicó: no hay forma de acusar.
            self.stats.ack_send_failed(peer_id, n);
            let total = self
                .stats
                .peer(peer_id)
                .ack_send_failed
                .load(std::sync::atomic::Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(total.saturating_sub(n)) {
                warn!(
                    peer = peer_id,
                    n,
                    why,
                    failed_total = total,
                    "ack: sin ack_endpoint del emisor y sin ruta etsi020; el peer verá expirar"
                );
            }
            return;
        }
        match self.inner.send_batch(addr, &batch).await {
            Ok(()) => {
                let before = self
                    .stats
                    .peer(peer_id)
                    .ack_sent
                    .fetch_add(n, std::sync::atomic::Ordering::Relaxed);
                if before == 0 {
                    info!(
                        peer = peer_id,
                        ack_endpoint = %addr,
                        "dkms.ack_socket: primer ACK enviado a este peer",
                    );
                }
                debug!(peer = peer_id, ack_endpoint = %addr, n, why, "ack_socket batch enviado");
            }
            Err(e) => {
                self.stats.ack_send_failed(peer_id, n);
                // Contamos claves, no intentos: es lo que el emisor verá
                // expirar al otro lado.
                warn!(
                    peer = peer_id,
                    ack_endpoint = %addr,
                    error = %e,
                    keys_perdidas = n,
                    why,
                    "ack_socket: no pude entregar el ACK; el peer verá estas claves expirar",
                );
            }
        }
    }

    async fn flush_all(&self) {
        let snapshot: Vec<(String, String, Vec<String>)> = {
            let mut g = self.pending.lock();
            g.iter_mut()
                .filter(|(_, (_, v))| !v.is_empty())
                .map(|(peer, (addr, v))| (peer.clone(), addr.clone(), std::mem::take(v)))
                .collect()
        };
        // 2026-05-24: paralelizamos el flush por peer. Antes el bucle
        // era secuencial y un peer lento (connect_timeout 2 s) bloqueaba
        // los demás; con 19 peers en BA el flush llegaba a 38 s,
        // superando el ack_timeout_ms del sender (30 s), que entonces
        // borraba ack_pending con reaper → Generator::on_ack no-op →
        // enc no crecía → observed_rate=0 con 72/380 commodities en BA.
        let tasks = snapshot.into_iter().map(|(peer, addr, batch)| {
            let me = self.clone();
            tokio::spawn(async move {
                me.send_and_count(&peer, &addr, batch, "tick").await;
            })
        });
        futures::future::join_all(tasks).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn ack_from_gate_rejects_unknown_or_overlong() {
        assert!(ack_from_is_processable("dkms-2", true));
        // Peer desconocido: descartado (no crea estado).
        assert!(!ack_from_is_processable("dkms-9", false));
        // `from` absurdamente largo: descartado aunque fuera "conocido".
        let huge = "x".repeat(MAX_ACK_FROM + 1);
        assert!(!ack_from_is_processable(&huge, true));
    }

    #[tokio::test]
    async fn read_bounded_line_rejects_an_overlong_line() {
        // Línea normal terminada en '\n': aceptada.
        let data = b"{\"from\":\"d\",\"key_ids\":[]}\n".to_vec();
        let mut r = BufReader::new(&data[..]);
        let mut buf = String::new();
        let out = read_bounded_line(&mut r, &mut buf, 1024).await.unwrap();
        assert_eq!(out, Some(buf.len()));
        // Línea de 5000 B sin '\n' con tope 1024: rechazada (None) — sin esto
        // crecería un String sin límite pre-auth (B3).
        let big = vec![b'A'; 5000];
        let mut r = BufReader::new(&big[..]);
        let mut buf = String::new();
        let out = read_bounded_line(&mut r, &mut buf, 1024).await.unwrap();
        assert!(out.is_none(), "una línea > tope debe rechazarse");
    }
}
