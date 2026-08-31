//! Listener TCP que acepta frames de **otros QKCs**.
//!
//! Tipos manejados:
//!
//! * `FRAME_RECV` / `FRAME_RELAY` → al módulo `relay`.
//! * `FRAME_KEY_IDS_NOTIFY` → al `KeyStore` del enlace correspondiente
//!   (rellena buffer DEC).
//!
//! Cada frame de datos se despacha en su propia task tokio para no
//! serializar, pero la ENTRADA está acotada: el reader hace `try_send` a una
//! cola global y un dispatcher adquiere el permiso del semáforo ANTES de
//! spawnear. Así la memoria pendiente es `INTAKE_QUEUE` frames +
//! `MAX_INFLIGHT_PEER` tasks — antes el spawn iba delante del permiso y un
//! peer que nos superara acumulaba tasks (cada una con su Frame) sin tope,
//! la misma clase de OOM que el writer documenta en `peer_client.rs`. A cola
//! llena el frame se DESCARTA contado (`intake_dropped_full` en `/stats`):
//! esta capa no retransmite y el material OTP de un frame parado se quema
//! igual; la alternativa real al drop es el OOM del runtime, que lo pierde
//! todo.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::{net::TcpStream, sync::Semaphore};
use tracing::{debug, info, warn};
use uuid::Uuid;
use wire::{
    decode_notify_payload, read_frame, FRAME_KEY_IDS_NOTIFY, FRAME_KEY_IDS_NOTIFY_AUTH,
    FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_INIT_AUTH, FRAME_PQC_KEM_INIT_SIGNED, FRAME_PQC_KEM_RESP,
    FRAME_PQC_KEM_RESP_AUTH, FRAME_PQC_KEM_RESP_SIGNED, FRAME_PQC_RESYNC_REQ,
    FRAME_PQC_RESYNC_REQ_AUTH, FRAME_PQC_RESYNC_REQ_SIGNED, FRAME_RECV, FRAME_RECV_AUTH,
    FRAME_RELAY, FRAME_RELAY_AUTH,
};

use crate::{
    pqc_handshake::{HsMsg, RecvAuth},
    relay,
    service::QkcService,
};

/// Máximo de frames `FRAME_RECV`/`FRAME_RELAY` simultáneos en vuelo
/// por proceso. Backpressurea al peer si nos manda más rápido de lo
/// que podemos descifrar+recifrar. Sin esto, en el hub de una estrella
/// con 4 ramas la concurrencia llega a miles → tareas waiting en
/// `wait_dec` hasta saturar memoria del runtime.
const MAX_INFLIGHT_PEER: usize = 8192;

/// Frames de datos aparcados entre el reader y el dispatcher. Con el
/// semáforo lleno el dispatcher deja de consumir, la cola se llena y el
/// reader descarta contado — memoria acotada por construcción.
const INTAKE_QUEUE: usize = 8192;

pub async fn serve(svc: QkcService, addr: &str) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = addr.parse()?;
    let listener = common::net::bind_reuse_addr(addr).await?;
    let inflight = Arc::new(Semaphore::new(MAX_INFLIGHT_PEER));
    let (intake_tx, mut intake_rx) = tokio::sync::mpsc::channel::<wire::Frame>(INTAKE_QUEUE);
    info!(
        %addr,
        max_inflight = MAX_INFLIGHT_PEER,
        intake_queue = INTAKE_QUEUE,
        "qkc.peer_server.listening"
    );
    // Dispatcher único: permiso ANTES de spawnear, así el nº de tasks vivas
    // lo acota el semáforo y lo pendiente lo acota la cola.
    {
        let svc = svc.clone();
        tokio::spawn(async move {
            while let Some(frame) = intake_rx.recv().await {
                let permit = match Arc::clone(&inflight).acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let svc2 = svc.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = relay::handle_incoming(svc2, frame).await {
                        // Throttled: aquí desemboca TODO error sostenido del
                        // relay (BadMac, replay, plaintext rechazado, enlace
                        // seco) — un warn por frame re-anulaba el throttling
                        // de los sitios de origen (la clase 752 MB/10 min).
                        static N: AtomicU64 = AtomicU64::new(0);
                        let n = N.fetch_add(1, Ordering::Relaxed);
                        if common::log_throttle::nth_is_loud(n) {
                            warn!(error = %e, total = n + 1, "qkc.relay.handle_err");
                        }
                    }
                });
            }
        });
    }
    loop {
        let (stream, peer) = listener.accept().await?;
        let _ = stream.set_nodelay(true);
        debug!(%peer, "qkc.peer_server.accept");
        let svc = svc.clone();
        let intake = intake_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(svc, stream, intake).await {
                debug!(error = %e, "qkc.peer_server.conn_ended");
            }
        });
    }
}

async fn handle_conn(
    svc: QkcService,
    mut stream: TcpStream,
    intake: tokio::sync::mpsc::Sender<wire::Frame>,
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
            // Las variantes `_AUTH` siguen exactamente el mismo camino: el MAC
            // se comprueba en `relay::handle_incoming`, donde el enlace (y por
            // tanto la clave) ya está resuelto. Aquí sólo hay que dejarlas pasar.
            FRAME_RECV | FRAME_RELAY | FRAME_RECV_AUTH | FRAME_RELAY_AUTH => {
                // try_send, nunca await: si el reader se bloquease, el
                // siguiente frame (que podría ser `FRAME_KEY_IDS_NOTIFY` con
                // las claves que un handler de delante espera) no se leería
                // → head-of-line deadlock. La concurrencia real la acota el
                // semáforo en el dispatcher; lo pendiente, la cola.
                enqueue_data_frame(&intake, &svc.stats, frame);
            }
            FRAME_KEY_IDS_NOTIFY => {
                // Sin semáforo: handle_notify es síncrono y trivial
                // (un push a Mutex<Vec> + notify_one). NO debe nunca
                // bloquear el flujo de datos por backpressure.
                handle_notify(svc.clone(), frame);
            }
            FRAME_KEY_IDS_NOTIFY_AUTH => handle_notify(svc.clone(), frame),
            // Handshake ML-KEM de enlaces PQC. Síncrono (encap/decap son
            // µs de CPU); los frames de un peer se procesan en orden, así
            // que no hay encap/decap concurrentes para un mismo enlace.
            FRAME_PQC_KEM_INIT => handle_pqc(svc.clone(), frame, HsMsg::Init, RecvAuth::Plain),
            FRAME_PQC_KEM_RESP => handle_pqc(svc.clone(), frame, HsMsg::Resp, RecvAuth::Plain),
            FRAME_PQC_KEM_INIT_AUTH => handle_pqc(svc.clone(), frame, HsMsg::Init, RecvAuth::Hmac),
            FRAME_PQC_KEM_RESP_AUTH => handle_pqc(svc.clone(), frame, HsMsg::Resp, RecvAuth::Hmac),
            FRAME_PQC_KEM_INIT_SIGNED => {
                handle_pqc(svc.clone(), frame, HsMsg::Init, RecvAuth::Signed)
            }
            FRAME_PQC_KEM_RESP_SIGNED => {
                handle_pqc(svc.clone(), frame, HsMsg::Resp, RecvAuth::Signed)
            }
            FRAME_PQC_RESYNC_REQ => {
                handle_pqc(svc.clone(), frame, HsMsg::ResyncReq, RecvAuth::Plain)
            }
            FRAME_PQC_RESYNC_REQ_AUTH => {
                handle_pqc(svc.clone(), frame, HsMsg::ResyncReq, RecvAuth::Hmac)
            }
            FRAME_PQC_RESYNC_REQ_SIGNED => {
                handle_pqc(svc.clone(), frame, HsMsg::ResyncReq, RecvAuth::Signed)
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
fn handle_notify(svc: QkcService, mut frame: wire::Frame) {
    let sender = frame.sender_id;
    let Some(link) = svc.link_to(sender) else {
        warn!(sender, "qkc.notify: unknown neighbor");
        return;
    };
    // Mismo camino que los frames de datos: MAC, frescura y trailer fuera. Es
    // lo único que impide que un tercero decida qué `key_ID` pide este QKC a su
    // KME — el material QKD protege el contenido, no quién pide qué.
    if let Err(e) = crate::frame_auth::authenticate(link.frame_auth.as_ref(), &mut frame) {
        warn!(sender, error = %e, "qkc.notify: rechazado");
        return;
    }
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

/// Handshake ML-KEM de un enlace PQC. `Init` → somos respondedor (payload =
/// pubkey); `Resp` → somos iniciador (payload = ciphertext); `ResyncReq` →
/// el respondedor nos pide renegociar por encima de su ventana.
/// Encola un frame de datos hacia el dispatcher. `false` = descartado (cola
/// llena o dispatcher muerto), contado en `intake_dropped_full` y con warn
/// throttled — nunca bloquea al reader.
fn enqueue_data_frame(
    intake: &tokio::sync::mpsc::Sender<wire::Frame>,
    stats: &crate::service::ServiceStats,
    frame: wire::Frame,
) -> bool {
    match intake.try_send(frame) {
        Ok(()) => true,
        Err(_) => {
            let total = stats.intake_dropped_full.fetch_add(1, Ordering::Relaxed);
            static N: AtomicU64 = AtomicU64::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    total = total + 1,
                    "qkc.peer_server.intake_full: frame de datos descartado (contrapresión)"
                );
            }
            false
        }
    }
}

fn handle_pqc(svc: QkcService, frame: wire::Frame, msg: HsMsg, recv: RecvAuth) {
    let sender = frame.sender_id;
    let Some(link) = svc.link_to(sender) else {
        warn!(sender, "qkc.pqc: unknown neighbor");
        return;
    };
    let Some(pqc) = &link.pqc else {
        warn!(sender, "qkc.pqc: frame on non-PQC link, ignoring");
        return;
    };
    match msg {
        HsMsg::Init => pqc.handle_init(&frame.payload, recv),
        HsMsg::Resp => pqc.handle_resp(&frame.payload, recv),
        HsMsg::ResyncReq => pqc.handle_resync_request(&frame.payload, recv),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cola llena el frame se descarta contado, sin bloquear jamás.
    #[tokio::test]
    async fn full_intake_drops_and_counts() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<wire::Frame>(1);
        let stats = crate::service::ServiceStats::default();
        assert!(enqueue_data_frame(
            &tx,
            &stats,
            wire::Frame::empty(FRAME_RECV)
        ));
        assert!(!enqueue_data_frame(
            &tx,
            &stats,
            wire::Frame::empty(FRAME_RECV)
        ));
        assert_eq!(
            stats.intake_dropped_full.load(Ordering::Relaxed),
            1,
            "el drop se cuenta"
        );
        // Al vaciar la cola vuelve a aceptar.
        rx.recv().await.expect("frame encolado");
        assert!(enqueue_data_frame(
            &tx,
            &stats,
            wire::Frame::empty(FRAME_RECV)
        ));
    }
}
