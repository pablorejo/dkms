//! Conexión TCP persistente hacia el QKC co-localizado.
//!
//! Wire = el binario de `wire` (mismo que ORR↔QKC en el QKC Python).
//! Bidireccional:
//!
//! * **ORR → QKC**: `FRAME_LOCAL_SEND`. Lo dispara [`QkcLink::send`].
//! * **QKC → ORR**: `FRAME_LOCAL_DELIVER`. El supervisor los toma y los
//!   inyecta por un `mpsc` en el servicio ORR.
//!
//! Resiliencia: una sola conexión activa, supervisada en un task. Si
//! cae (error de lectura/escritura o EOF), el supervisor descarta los
//! frames en vuelo, hace backoff exponencial acotado y vuelve a
//! conectar. Los productores que hagan `send` durante el corte
//! recibirán `Err` solo si la cola interna se llena — mientras quepa,
//! se encolarán y se intentarán mandar en la siguiente sesión.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use wire::{read_frame, write_frame, Frame, FRAME_LOCAL_DELIVER};

use crate::error::{OrrError, Result};

/// Capacidad de la cola de envío hacia el QKC. Si se llena (QKC
/// saturado o caído), los `send` aplican backpressure.
const SEND_QUEUE_CAPACITY: usize = 4096;

/// Frame entrante que el QKC nos entregó (kind = `FRAME_LOCAL_DELIVER`).
pub type DeliveryTx = mpsc::Sender<Frame>;
pub type DeliveryRx = mpsc::Receiver<Frame>;

/// Cliente persistente hacia el QKC local. Cloneable; clones comparten
/// la misma cola de envío y el mismo supervisor.
#[derive(Clone)]
pub struct QkcLink {
    inner: Arc<LinkInner>,
}

struct LinkInner {
    addr:       String,
    send_tx:    mpsc::Sender<Frame>,
    /// Sender que el reader usa para inyectar deliveries. `Mutex` para
    /// poder rotarlo en escenarios de test.
    deliveries: Mutex<Option<DeliveryTx>>,
}

impl QkcLink {
    /// Arranca el supervisor del link. `deliveries` recibirá los frames
    /// `FRAME_LOCAL_DELIVER` que vengan del QKC.
    pub fn spawn(addr: String, deliveries: DeliveryTx) -> Self {
        let (send_tx, send_rx) = mpsc::channel::<Frame>(SEND_QUEUE_CAPACITY);
        let inner = Arc::new(LinkInner {
            addr: addr.clone(),
            send_tx,
            deliveries: Mutex::new(Some(deliveries)),
        });
        let supervisor_inner = inner.clone();
        tokio::spawn(supervisor_loop(supervisor_inner, send_rx));
        Self { inner }
    }

    /// Encola un frame para enviar al QKC. Aplica backpressure si la
    /// cola está llena. Devuelve `Err` solo si el supervisor ha
    /// terminado (lo cual no debería pasar en proceso vivo).
    pub async fn send(&self, frame: Frame) -> Result<()> {
        self.inner
            .send_tx
            .send(frame)
            .await
            .map_err(|_| OrrError::Relay("qkc link send channel closed".into()))
    }

    /// Reemplaza el canal de entregas. En producción solo se llama una
    /// vez vía `spawn`; está expuesto para tests.
    pub fn set_deliveries(&self, deliveries: DeliveryTx) {
        *self.inner.deliveries.lock() = Some(deliveries);
    }
}

async fn supervisor_loop(inner: Arc<LinkInner>, mut send_rx: mpsc::Receiver<Frame>) {
    let mut backoff_ms: u64 = 200;
    loop {
        let stream = match TcpStream::connect(&inner.addr).await {
            Ok(s) => s,
            Err(e) => {
                warn!(addr = %inner.addr, error = %e, "orr.qkc_link.connect_err");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(5_000);
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        info!(addr = %inner.addr, "orr.qkc_link.connected");
        backoff_ms = 200;

        let deliveries = inner.deliveries.lock().clone();
        run_session(stream, &mut send_rx, deliveries).await;

        warn!(addr = %inner.addr, "orr.qkc_link.disconnected — reconnecting");
        // Frames que entraron a la cola mientras estábamos en `run_session`
        // pero ya con la conexión rota: no podemos garantizar entrega,
        // así que los logueamos como dropped (semántica best-effort).
        while let Ok(dropped) = send_rx.try_recv() {
            debug!(kind = dropped.kind, "orr.qkc_link.drop_pending_frame");
        }
        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
    }
}

/// Pulsa una sola sesión TCP.
///
/// Read y write corren en tasks separadas (no en `tokio::select!` sobre
/// la misma stream) porque `read_frame` usa `read_exact` internamente,
/// que NO es cancel-safe: si `select!` lo cancela mid-read, los bytes
/// ya consumidos del stream se pierden y el siguiente `read_frame` ve
/// el cuerpo del frame anterior y falla con `bad magic`. Fue lo que
/// rompía mode 0 a alto throughput (orr_22/orr_11 perdían ~50 % de
/// frames con disconnects intermitentes).
///
/// El writer corre en el task actual; `select!` aquí solo combina dos
/// futuros cancel-safe: `send_rx.recv()` (mpsc) y `&mut read_task`
/// (JoinHandle). Si el reader cae, salimos. Si el writer falla, el
/// reader se aborta antes de retornar.
async fn run_session(
    stream: TcpStream,
    send_rx: &mut mpsc::Receiver<Frame>,
    deliveries: Option<DeliveryTx>,
) {
    let (mut read_half, mut write_half) = stream.into_split();

    let mut read_task = tokio::spawn(async move {
        loop {
            match read_frame(&mut read_half).await {
                Ok(frame) => {
                    if frame.kind != FRAME_LOCAL_DELIVER {
                        debug!(kind = frame.kind, "orr.qkc_link.unexpected_kind");
                        continue;
                    }
                    if let Some(tx) = deliveries.as_ref() {
                        if tx.send(frame).await.is_err() {
                            warn!("orr.qkc_link.deliveries_closed");
                            return;
                        }
                    }
                }
                Err(wire::WireError::Io(e))
                    if e.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    debug!("orr.qkc_link.eof");
                    return;
                }
                Err(e) => {
                    warn!(error = %e, "orr.qkc_link.read_err");
                    return;
                }
            }
        }
    });

    loop {
        tokio::select! {
            outgoing = send_rx.recv() => {
                let Some(frame) = outgoing else { break; };
                if let Err(e) = write_frame(&mut write_half, &frame).await {
                    warn!(error = %e, "orr.qkc_link.write_err");
                    break;
                }
            }
            _ = &mut read_task => {
                // Reader terminó (EOF, error). Salir para reconectar.
                break;
            }
        }
    }

    if !read_task.is_finished() {
        read_task.abort();
    }
    let _ = read_task.await;
}
