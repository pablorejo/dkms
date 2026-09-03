//! Listener TCP **local** para conexiones del ORR.
//!
//! Cada conexión es **bidi**:
//!
//! * **ORR → QKC**: frames `FRAME_LOCAL_SEND` con plaintext. El QKC
//!   los cifra y los reenvía según la forwarding table.
//! * **QKC → ORR**: frames `FRAME_LOCAL_DELIVER` con plaintext. El QKC
//!   los empuja por la misma conexión cuando llega un `RECV` cuyo
//!   `dest_final == my_id`.
//!
//! Mismo wire binario (`wire::Frame`) que QKC↔QKC — ningún parser
//! distinto, solo `kind` distinto.

static ACCEPT_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::{
    net::TcpStream,
    sync::{mpsc, Semaphore},
};
use tracing::{debug, info, warn};
use wire::{read_frame, write_frame, Frame, FRAME_LOCAL_SEND};

use crate::{relay, service::QkcService};

/// Tamaño de la cola mpsc entre `deliver_local` y el writer task que
/// escribe a la TCP local del ORR. Si se llena, `try_send` devuelve
/// `Full` y `deliver_local` DROPEA el frame silenciosamente.
///
/// Con ráfagas all-to-all en una estrella de 4 hojas un leaf puede
/// recibir ~15K frames en ~1 s. 65536 da margen para ~4 s de
/// backlog antes de empezar a dropear — suficiente para flushear
/// la TCP del listener.
const DELIVER_QUEUE_CAPACITY: usize = 65536;

/// Máximo de `FRAME_LOCAL_SEND` simultáneamente en vuelo POR PROCESO.
/// Sirve de backpressure contra un ORR que vomite miles de frames de
/// golpe. Con `wait_enc_batch` el hot path ya no hace HTTP propio, así
/// que el coste por tarea es bajo y podemos permitir mucha
/// concurrencia. El cap real es la velocidad del worker enc + TCP del
/// peer_out.
const MAX_INFLIGHT_LOCAL_SEND: usize = 4096;

pub async fn serve(svc: QkcService, addr: &str) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = addr.parse()?;
    let listener = common::net::bind_reuse_addr(addr).await?;
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_LOCAL_SEND));
    info!(%addr, max_inflight = MAX_INFLIGHT_LOCAL_SEND, "qkc.local.listening");
    loop {
        // Un error transitorio de accept (EMFILE) no puede tumbar el plano
        // entero (auditoría 2026-09b E2 / 2026-09-03 R8): se cuenta, se
        // avisa en potencias de dos y se espera un poco.
        let (stream, peer) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                let n = ACCEPT_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n) {
                    warn!(error = %e, failed = n + 1, "qkc.local: accept failed");
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        debug!(%peer, "qkc.local.accept");
        let svc = svc.clone();
        let inflight = Arc::clone(&inflight);
        tokio::spawn(async move {
            handle_conn(svc, stream, inflight).await;
        });
    }
}

async fn handle_conn(svc: QkcService, stream: TcpStream, inflight: Arc<Semaphore>) {
    let (mut read_half, mut write_half) = stream.into_split();

    // Sender hacia esta conexión local. Lo registramos en el service
    // para que `deliver_local` empuje frames LOCAL_DELIVER aquí.
    let (tx, mut rx) = mpsc::channel::<Frame>(DELIVER_QUEUE_CAPACITY);
    svc.register_local(tx);

    // Writer task: drena rx → escribe al socket.
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if let Err(e) = write_frame(&mut write_half, &frame).await {
                warn!(error = %e, "qkc.local.write_err");
                break;
            }
        }
    });

    // Reader: bucle leyendo frames del ORR.
    loop {
        let frame = match read_frame(&mut read_half).await {
            Ok(f) => f,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => {
                warn!(error = %e, "qkc.local.frame_err");
                break;
            }
        };
        if frame.kind == FRAME_LOCAL_SEND {
            // Adquirir el permiso ANTES de leer el siguiente frame.
            // Cuando los MAX_INFLIGHT_LOCAL_SEND tasks estén ocupados,
            // el reader bloquea aquí → TCP recv buffer se llena → el
            // writer del ORR (kernel-side) se bloquea → su mpsc de
            // qkc_link no drena → SendMessage espera en el ORR →
            // backpressure llega al sender. Sin esto el reader drenaba
            // TCP a tope y los tasks pendientes esperaban > 5s por la
            // key QKD, fallando con KeyWaitTimeout y dropeando frames
            // silenciosamente (`local_send_errs`).
            let permit = match inflight.clone().acquire_owned().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let svc2 = svc.clone();
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = relay::handle_local_send(svc2, frame).await {
                    // Throttled: con el ENC seco esto era un warn por frame.
                    static N: AtomicU64 = AtomicU64::new(0);
                    let n = N.fetch_add(1, Ordering::Relaxed);
                    if common::log_throttle::nth_is_loud(n) {
                        warn!(error = %e, total = n + 1, "qkc.local.send_err");
                    }
                }
            });
        } else {
            debug!(kind = frame.kind, "qkc.local.unexpected_kind");
        }
    }

    // Aborta el writer ANTES de purgar la lista. Cuando el writer
    // task dropea su `rx`, el `tx` que vive en `local_out` queda
    // marcado como `is_closed`, y `prune_local()` ya puede eliminarlo
    // de la lista. Si purgásemos antes del abort, el tx aún se
    // contaría como vivo y los próximos `deliver_local` lo seguirían
    // intentando — generando `deliver_drops_closed`.
    writer.abort();
    let _ = writer.await;
    svc.prune_local();
}
