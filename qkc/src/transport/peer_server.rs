//! Listener TCP que acepta frames de **otros QKCs**.
//!
//! Tipos manejados:
//!
//! * `FRAME_RECV` / `FRAME_RELAY` → al módulo `relay`.
//! * `FRAME_KEY_IDS_NOTIFY` → al `KeyStore` del enlace correspondiente
//!   (rellena buffer DEC).
//!
//! Cada frame se despacha en su propia task tokio para no serializar.

use std::sync::Arc;

use tokio::{net::TcpStream, sync::Semaphore};
use tracing::{debug, info, warn};
use uuid::Uuid;
use wire::{decode_notify_payload, read_frame, FRAME_KEY_IDS_NOTIFY, FRAME_RECV, FRAME_RELAY};

use crate::{relay, service::QkcService};

/// Máximo de frames `FRAME_RECV`/`FRAME_RELAY` simultáneos en vuelo
/// por proceso. Backpressurea al peer si nos manda más rápido de lo
/// que podemos descifrar+recifrar. Sin esto, en el hub de una estrella
/// con 4 ramas la concurrencia llega a miles → tareas waiting en
/// `wait_dec` hasta saturar memoria del runtime.
const MAX_INFLIGHT_PEER: usize = 8192;

pub async fn serve(svc: QkcService, addr: &str) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = addr.parse()?;
    let listener = common::net::bind_reuse_addr(addr).await?;
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_PEER));
    info!(%addr, max_inflight = MAX_INFLIGHT_PEER, "qkc.peer_server.listening");
    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        debug!(%peer, "qkc.peer_server.accept");
        let svc = svc.clone();
        let inflight = Arc::clone(&inflight);
        tokio::spawn(async move {
            if let Err(e) = handle_conn(svc, stream, inflight).await {
                debug!(error = %e, "qkc.peer_server.conn_ended");
            }
        });
    }
}

async fn handle_conn(
    svc: QkcService,
    mut stream: TcpStream,
    inflight: Arc<Semaphore>,
) -> anyhow::Result<()> {
    loop {
        let frame = match read_frame(&mut stream).await {
            Ok(f) => f,
            Err(wire::WireError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Ok(());
            }
            Err(e) => {
                warn!(error = %e, "qkc.peer_server.frame_err");
                return Err(e.into());
            }
        };

        match frame.kind {
            FRAME_RECV | FRAME_RELAY => {
                // Spawn ANTES de adquirir el permiso: si bloqueamos el
                // reader esperando un permiso, el siguiente frame
                // (que podría ser `FRAME_KEY_IDS_NOTIFY` con las claves
                // que el handler de delante está esperando) nunca se
                // lee → head-of-line deadlock.
                //
                // El semáforo sigue limitando la concurrencia real de
                // `handle_incoming`: el spawn es barato, pero cada
                // tarea hace `acquire_owned().await` antes de empezar.
                let svc2 = svc.clone();
                let inflight = Arc::clone(&inflight);
                tokio::spawn(async move {
                    let _permit = match inflight.acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => return,
                    };
                    if let Err(e) = relay::handle_incoming(svc2, frame).await {
                        warn!(error = %e, "qkc.relay.handle_err");
                    }
                });
            }
            FRAME_KEY_IDS_NOTIFY => {
                // Sin semáforo: handle_notify es síncrono y trivial
                // (un push a Mutex<Vec> + notify_one). NO debe nunca
                // bloquear el flujo de datos por backpressure.
                handle_notify(svc.clone(), frame);
            }
            other => {
                debug!(kind = other, "qkc.peer_server.unknown_kind");
            }
        }
    }
}

/// `FRAME_KEY_IDS_NOTIFY`: el peer (sender_id) acaba de pedir N claves
/// con estos UUIDs al quditto compartido del enlace; nosotros debemos
/// pedirlos a nuestro `dec_keys` para llenar nuestro buffer DEC.
fn handle_notify(svc: QkcService, frame: wire::Frame) {
    let sender = frame.sender_id;
    let Some(link) = svc.link_to(sender) else {
        warn!(sender, "qkc.notify: unknown neighbor");
        return;
    };
    let raw_ids = match decode_notify_payload(&frame.payload) {
        Ok(v) => v,
        Err(e) => {
            warn!(sender, error = %e, "qkc.notify: bad payload");
            return;
        }
    };
    let ids: Vec<Uuid> = raw_ids.into_iter().map(Uuid::from_bytes).collect();
    let n = ids.len();
    link.keys.notify_remote_enc(ids);
    debug!(sender, ids = n, "qkc.notify");
}
