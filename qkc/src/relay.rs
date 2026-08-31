//! Lógica de enrutado y crypto.
//!
//! Tres entrypoints:
//!
//! * [`handle_incoming`]: frame `FRAME_RECV` / `FRAME_RELAY` desde otro
//!   QKC. Descifra con claves del **link entrante** (lookup en
//!   `KeyStore.dec`, espera al worker si miss).
//!   - Si `dest_final == my_id` → entrega plaintext al ORR local.
//!   - Si no → recifra con claves del **link saliente** (sacando del
//!     `KeyStore.enc`) y envía al next-hop.
//!
//! * [`handle_local_send`]: frame `FRAME_LOCAL_SEND` desde el ORR
//!   local. Resuelve next-hop, cifra con `KeyStore.enc`, envía.
//!
//! Hot path **sin HTTP** (las claves vienen del buffer). El refill
//! ocurre en background (ver `keystore.rs`). Si el warm-up todavía no
//! ha llegado, el relay **espera** al `dec_refill_loop` (DEC) o al
//! `enc_refill_loop` (ENC). No hay fallback HTTP porque sería inviable:
//! el quditto ya entregó la clave al worker.

use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    sync::atomic::Ordering,
    time::Duration,
};

use uuid::Uuid;
use wire::{Frame, FRAME_LOCAL_DELIVER, FRAME_RECV, FRAME_RELAY, GRADE_QKD};

/// Stable hash for a frame used as the bucket key into a WCMP next-hop
/// entry. We pick fields that vary per-frame (so different keys hit
/// different next-hops, spreading load) but are stable across hops
/// (so the same logical frame keeps flow affinity at each relay).
///
/// `dest_final` + the two layered headers (`header_orr_mp`,
/// `header_dkms_mp`) survive byte-for-byte across hops. `key_ids`
/// change at every hop (we re-encrypt with fresh transport keys), so
/// they're useful only when the WCMP entry sits *before* re-encryption
/// — but folding them in is cheap and doesn't hurt flow affinity for
/// frames where they happen to be empty.
fn frame_hash(frame: &Frame) -> u64 {
    let mut h = DefaultHasher::new();
    frame.dest_final.hash(&mut h);
    frame.header_orr_mp.hash(&mut h);
    frame.header_dkms_mp.hash(&mut h);
    for id in &frame.key_ids {
        id.hash(&mut h);
    }
    h.finish()
}

use crate::{
    crypto::{decrypt, encrypt, num_chunks},
    error::{QkcError, Result},
    kme::OtpKey,
    service::{LinkRuntime, QkcService},
};

/// Tiempo máximo de espera por una clave DEC concreta. Si el worker no
/// la inserta en este plazo, dropeamos el frame.
const DEC_WAIT_TIMEOUT: Duration = Duration::from_millis(2000);
/// Tiempo máximo de espera por un batch completo de claves ENC al
/// pedirle al worker que rellene. Más grande que DEC porque puede
/// requerir varias rondas HTTP a quditto. Subido a 30s porque bajo
/// régimen QKD-limited el race del `notify_waiters` no es FIFO y los
/// tasks unlucky pueden esperar varias veces el promedio.
const ENC_WAIT_TIMEOUT: Duration = Duration::from_millis(30000);

/// Frame entrante desde otro QKC.
pub async fn handle_incoming(svc: QkcService, frame: Frame) -> Result<()> {
    svc.stats.incoming_starts.fetch_add(1, Ordering::Relaxed);
    let result = handle_incoming_inner(&svc, frame).await;
    match &result {
        Ok(()) => {}
        Err(_) => {
            svc.stats.incoming_errs.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

async fn handle_incoming_inner(svc: &QkcService, mut frame: Frame) -> Result<()> {
    let in_link = svc
        .link_to(frame.sender_id)
        .ok_or(QkcError::UnknownNeighbor(frame.sender_id))?;
    // Autenticación del frame ANTES de tocar nada: `sender_id` todavía no está
    // verificado, así que hasta aquí sólo se ha usado para elegir el enlace (y
    // por tanto la clave con la que se comprueba). Un `open` correcto es lo que
    // ata el frame a ese enlace.
    crate::frame_auth::authenticate(in_link.frame_auth.as_ref(), &mut frame)?;
    let in_chunk_bytes = (in_link.cfg.key_size_bits / 8) as usize;
    let key_ids: Vec<Uuid> = parse_key_ids(&frame.key_ids)?;

    // Resolver claves de descifrado del buffer DEC, esperando al worker
    // si miss (warm-up race del NOTIFY ↔ frame).
    let dec_keys = lookup_or_fetch_dec(&in_link, &key_ids).await?;
    let plaintext = decrypt(&frame.payload, &key_ids, &dec_keys, in_chunk_bytes)?;

    // ¿Local-deliver o forward? En ambos casos el QKC NO toca
    // header_orr_mp ni header_dkms_mp — los propaga byte-a-byte. Solo
    // (el QKC no añade headers; sólo propaga orr_mp + dkms_mp en el siguiente
    // hop).
    if frame.dest_final == svc.qkc_id() {
        let qkc_id = svc.qkc_id();
        let out = local_deliver_frame(qkc_id, frame, plaintext);
        svc.deliver_local(out);
        svc.stats.incoming_delivered.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    let hash = frame_hash(&frame);
    let grade = frame.grade;
    let next_hop = svc
        .routing
        .next_hop_graded(frame.dest_final, hash, grade == GRADE_QKD)
        .ok_or(QkcError::NoRoute(frame.dest_final))?;
    forward_plaintext(
        svc,
        next_hop,
        frame.dest_final,
        &plaintext,
        grade,
        frame.epoch_id,
        frame.header_orr_mp,
        frame.header_dkms_mp,
    )
    .await?;
    svc.stats.incoming_forwarded.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Frame entrante desde el ORR local (plaintext a enviar).
pub async fn handle_local_send(svc: QkcService, frame: Frame) -> Result<()> {
    svc.stats.local_send_starts.fetch_add(1, Ordering::Relaxed);
    let result = handle_local_send_inner(&svc, frame).await;
    match &result {
        Ok(()) => {
            svc.stats.local_send_oks.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            svc.stats.local_send_errs.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

async fn handle_local_send_inner(svc: &QkcService, frame: Frame) -> Result<()> {
    let dest = frame.dest_final;
    if dest == svc.qkc_id() {
        return Err(QkcError::BadRequest(
            "LOCAL_SEND con dest_final == my_id no tiene sentido".into(),
        ));
    }
    let hash = frame_hash(&frame);
    let grade = frame.grade;
    let next_hop = svc
        .routing
        .next_hop_graded(dest, hash, grade == GRADE_QKD)
        .ok_or(QkcError::NoRoute(dest))?;
    forward_plaintext(
        svc,
        next_hop,
        dest,
        &frame.payload,
        grade,
        frame.epoch_id,
        frame.header_orr_mp,
        frame.header_dkms_mp,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn forward_plaintext(
    svc: &QkcService,
    next_hop: u32,
    dest_final: u32,
    plaintext: &[u8],
    grade: u8,
    epoch_id: u32,
    header_orr_mp: Vec<u8>,
    header_dkms_mp: Vec<u8>,
) -> Result<()> {
    let out_link = svc
        .link_to(next_hop)
        .ok_or(QkcError::UnknownNeighbor(next_hop))?;
    let chunk_bytes = (out_link.cfg.key_size_bits / 8) as usize;
    let needed = num_chunks(plaintext.len(), chunk_bytes);
    let enc_keys = take_or_fetch_enc(&out_link, needed).await?;
    let (ciphertext, ids) = encrypt(plaintext, &enc_keys)?;
    send_frame_to_peer(
        svc,
        &out_link,
        next_hop,
        dest_final,
        ciphertext,
        ids,
        grade,
        epoch_id,
        header_orr_mp,
        header_dkms_mp,
    )
}

#[allow(clippy::too_many_arguments)]
fn send_frame_to_peer(
    svc: &QkcService,
    out_link: &crate::service::LinkRuntime,
    next_hop: u32,
    dest_final: u32,
    ciphertext: Vec<u8>,
    key_ids: Vec<Uuid>,
    grade: u8,
    epoch_id: u32,
    header_orr_mp: Vec<u8>,
    header_dkms_mp: Vec<u8>,
) -> Result<()> {
    // El enlace llega YA resuelto de forward_plaintext: resolverlo dos veces
    // por frame era un load de ArcSwap + clone de Arc de más, y la dirección
    // sale de su propia config en vez de otra búsqueda + clone de String.
    let mut out = outbound_frame(
        svc.qkc_id(),
        next_hop,
        dest_final,
        grade,
        epoch_id,
        out_link.cfg.key_size_bits as u16,
        key_ids.iter().map(|u| u.to_string()).collect(),
        header_orr_mp,
        header_dkms_mp,
        ciphertext,
    );

    // El MAC va sobre el frame ya montado: cubre el ciphertext, las identidades
    // y los dos headers que el QKC propaga sin mirar. Después de esto el kind
    // pasa a su variante `_AUTH` y el payload lleva `session ‖ counter ‖ tag`.
    if let Some(fa) = &out_link.frame_auth {
        fa.seal(&mut out);
    }

    let ok = svc
        .peer_out
        .send(next_hop, &out_link.cfg.neighbor_peer_addr, out);
    if !ok {
        return Err(QkcError::Quditto(format!(
            "peer_out queue full for {next_hop}"
        )));
    }
    Ok(())
}

/// Lo que el QKC PROPAGA de un frame que atraviesa, sin mirarlo: los dos
/// headers de las capas de arriba, el grado, y **`epoch_id`** — la época del
/// `master_secret` con la que el ORR de destino pela la cebolla. No es del
/// QKC (el OTP del enlace lleva sus épocas dentro de los `key_id`), pero
/// viaja en el prefijo del wire y aquí es donde se perdía: los frames se
/// reconstruían con `Frame::empty` y llegaban siempre con época 0. Mientras
/// el ORR no rotaba nadie lo vio; en cuanto rotó, cada frame fallaba el tag
/// (medido 2026-08-30: `peel_failed` ≈ 25 % y re-bootstraps en bucle).
fn local_deliver_frame(qkc_id: u32, frame: Frame, plaintext: Vec<u8>) -> Frame {
    // Por valor: es el último uso del frame entrante, así que los dos headers
    // se MUEVEN en vez de clonarse (eran dos allocs por frame entregado).
    let mut out = Frame::empty(FRAME_LOCAL_DELIVER);
    out.sender_id = frame.sender_id;
    out.receiver_id = qkc_id;
    out.dest_final = qkc_id;
    out.grade = frame.grade;
    out.epoch_id = frame.epoch_id;
    out.key_size_bits = 0;
    out.key_ids = Vec::new();
    out.header_orr_mp = frame.header_orr_mp;
    out.header_dkms_mp = frame.header_dkms_mp;
    out.payload = plaintext;
    out
}

/// Frame hacia el siguiente QKC (`FRAME_RECV` si es el último salto,
/// `FRAME_RELAY` si no), con todo lo que se propaga (ver
/// [`local_deliver_frame`]) y lo propio del enlace de salida.
#[allow(clippy::too_many_arguments)]
fn outbound_frame(
    me: u32,
    next_hop: u32,
    dest_final: u32,
    grade: u8,
    epoch_id: u32,
    key_size_bits: u16,
    key_ids: Vec<String>,
    header_orr_mp: Vec<u8>,
    header_dkms_mp: Vec<u8>,
    ciphertext: Vec<u8>,
) -> Frame {
    let kind = if next_hop == dest_final {
        FRAME_RECV
    } else {
        FRAME_RELAY
    };
    let mut out = Frame::empty(kind);
    out.sender_id = me;
    out.receiver_id = next_hop;
    out.dest_final = dest_final;
    out.grade = grade;
    out.epoch_id = epoch_id;
    out.key_size_bits = key_size_bits;
    out.key_ids = key_ids;
    out.header_orr_mp = header_orr_mp;
    out.header_dkms_mp = header_dkms_mp;
    out.payload = ciphertext;
    out
}

fn parse_key_ids(strs: &[String]) -> Result<Vec<Uuid>> {
    let mut out = Vec::with_capacity(strs.len());
    for s in strs {
        let u = Uuid::parse_str(s)
            .map_err(|e| QkcError::BadRequest(format!("invalid key_id {s}: {e}")))?;
        out.push(u);
    }
    Ok(out)
}

// ─── Hot-path helpers: buffer first, espera al worker si miss ────────

/// Toma `n` claves del buffer ENC. Si no hay suficientes, espera a que
/// el `enc_refill_loop` rellene (no hace un `enc_keys` HTTP propio:
/// eso duplicaría peticiones a quditto).
async fn take_or_fetch_enc(link: &LinkRuntime, n: usize) -> Result<Vec<OtpKey>> {
    link.keys
        .wait_enc_batch(n, ENC_WAIT_TIMEOUT)
        .await
        .map_err(|t| QkcError::KeyWaitTimeout {
            what: "enc",
            missing: t.missing,
            ms: ENC_WAIT_TIMEOUT.as_millis() as u64,
        })
}

/// Busca cada `key_id` en el buffer DEC. Para los que no estén, espera
/// a que el `dec_refill_loop` los inserte (el quditto ya entregó las
/// claves al worker — un fallback HTTP daría 404).
///
/// `lookup_dec` CONSUME (semántica OTP: mueve la clave fuera del mapa), así
/// que en un batch multi-clave un timeout en la clave i-ésima QUEMA las i-1
/// ya sacadas — se van con `out`. No se reinsertan a propósito: esos ids no
/// se vuelven a pedir jamás (el frame se pierde), y devolverlos al DashMap
/// sería una fuga sin lector. Hoy la ventana es vacía: el payload de diseño
/// son 32 B = 1 bloque = 1 clave (`needed == 1`); solo mordería si algún día
/// un payload superase `key_size_bits / 8`.
async fn lookup_or_fetch_dec(link: &LinkRuntime, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let mat = match link.keys.lookup_dec(id) {
            Some(v) => v,
            None => link
                .keys
                .wait_dec(id, DEC_WAIT_TIMEOUT)
                .await
                .map_err(|t| QkcError::KeyWaitTimeout {
                    what: "dec",
                    missing: t.missing,
                    ms: DEC_WAIT_TIMEOUT.as_millis() as u64,
                })?,
        };
        out.push(OtpKey {
            key_id: *id,
            material: mat,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La época del ORR y los headers de arriba atraviesan el QKC intactos,
    /// tanto al entregar en local como al reenviar. Es lo que hace posible
    /// que el ORR rote su `master_secret`.
    #[test]
    fn the_orr_epoch_and_headers_survive_the_qkc_hop() {
        let mut inbound = Frame::empty(FRAME_RECV);
        inbound.sender_id = 7;
        inbound.dest_final = 3;
        inbound.grade = 1;
        inbound.epoch_id = 42;
        inbound.header_orr_mp = vec![1, 2, 3];
        inbound.header_dkms_mp = vec![4, 5];

        let delivered = local_deliver_frame(3, inbound, vec![9; 32]);
        assert_eq!(delivered.kind, FRAME_LOCAL_DELIVER);
        assert_eq!((delivered.epoch_id, delivered.grade), (42, 1));
        assert_eq!(delivered.header_orr_mp, vec![1, 2, 3]);
        assert_eq!(delivered.header_dkms_mp, vec![4, 5]);
        assert_eq!((delivered.sender_id, delivered.dest_final), (7, 3));

        let relayed = outbound_frame(3, 5, 9, 1, 42, 256, vec![], vec![1], vec![2], vec![0; 32]);
        assert_eq!(relayed.kind, FRAME_RELAY);
        assert_eq!((relayed.epoch_id, relayed.grade), (42, 1));
        let last_hop = outbound_frame(3, 9, 9, 1, 42, 256, vec![], vec![1], vec![2], vec![0; 32]);
        assert_eq!(last_hop.kind, FRAME_RECV);
        assert_eq!(last_hop.epoch_id, 42);
    }

    #[test]
    fn parse_key_ids_round_trip() {
        let u1 = Uuid::new_v4();
        let u2 = Uuid::new_v4();
        let strs = vec![u1.to_string(), u2.to_string()];
        let parsed = parse_key_ids(&strs).unwrap();
        assert_eq!(parsed, vec![u1, u2]);
    }

    #[test]
    fn parse_key_ids_rejects_garbage() {
        let strs = vec!["not-a-uuid".to_string()];
        assert!(parse_key_ids(&strs).is_err());
    }
}
