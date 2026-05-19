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

use std::{sync::atomic::Ordering, time::Duration};

use uuid::Uuid;
use tracing::debug;
use wire::{Frame, FRAME_LOCAL_DELIVER, FRAME_RECV, FRAME_RELAY};

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

async fn handle_incoming_inner(svc: &QkcService, frame: Frame) -> Result<()> {
    let in_link = svc
        .link_to(frame.sender_id)
        .ok_or(QkcError::UnknownNeighbor(frame.sender_id))?;
    let in_chunk_bytes = (in_link.cfg.key_size_bits / 8) as usize;
    let key_ids: Vec<Uuid> = parse_key_ids(&frame.key_ids)?;

    // Resolver claves de descifrado del buffer DEC, esperando al worker
    // si miss (warm-up race del NOTIFY ↔ frame).
    let dec_keys = lookup_or_fetch_dec(in_link, &key_ids).await?;
    let plaintext = decrypt(&frame.payload, &key_ids, &dec_keys, in_chunk_bytes)?;

    // ¿Local-deliver o forward? En ambos casos el QKC NO toca
    // header_orr_mp ni header_dkms_mp — los propaga byte-a-byte. Solo
    // recompone su header_qkc_mp (vacío al ORR; nuevo en el siguiente
    // hop).
    if frame.dest_final == svc.qkc_id() {
        let mut out = Frame::empty(FRAME_LOCAL_DELIVER);
        out.sender_id = frame.sender_id;
        out.receiver_id = svc.qkc_id();
        out.dest_final = svc.qkc_id();
        out.key_size_bits = 0;
        out.key_ids = Vec::new();
        // header_qkc_mp se desecha al entregar al ORR (vacío).
        out.header_orr_mp = frame.header_orr_mp;
        out.header_dkms_mp = frame.header_dkms_mp;
        out.payload = plaintext;
        svc.deliver_local(out);
        svc.stats.incoming_delivered.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }

    // OBJ-007: source routing K-Splittable. Lógica encapsulada en
    // `resolve_next_hop` (compartida con `handle_local_send_inner`).
    let (next_hop, out_header_qkc_mp) =
        resolve_next_hop(&frame.header_qkc_mp, frame.dest_final, |d| {
            svc.routing.next_hop(d)
        })?;
    forward_plaintext(
        svc,
        next_hop,
        frame.dest_final,
        &plaintext,
        out_header_qkc_mp,
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
    let (next_hop, out_header_qkc_mp) =
        resolve_next_hop(&frame.header_qkc_mp, dest, |d| svc.routing.next_hop(d))?;
    forward_plaintext(
        svc,
        next_hop,
        dest,
        &frame.payload,
        out_header_qkc_mp,
        frame.header_orr_mp,
        frame.header_dkms_mp,
    )
    .await
}

/// OBJ-006/007: decide `next_hop` + `out_header_qkc_mp` para el frame
/// saliente. Si `incoming_header_qkc_mp` no está vacío y se puede
/// decodificar un `qkc_path` válido, usa el primer elemento como
/// next_hop y propaga el resto. En cualquier otro caso (vacío, decode
/// falla, path agotado tras pop pero no estamos en destino), cae al
/// `routing_fallback` y limpia el header (Vec::new()) para que QKCs
/// downstream tampoco intenten consumirlo.
///
/// El logging `tracing::debug` se hace en este helper para que los dos
/// callers (`handle_local_send_inner` y `handle_incoming_inner`)
/// dejen los mismos eventos.
fn resolve_next_hop(
    incoming_header_qkc_mp: &[u8],
    dest_final: u32,
    routing_fallback: impl FnOnce(u32) -> Option<u32>,
) -> Result<(u32, Vec<u8>)> {
    if incoming_header_qkc_mp.is_empty() {
        let nh = routing_fallback(dest_final).ok_or(QkcError::NoRoute(dest_final))?;
        return Ok((nh, Vec::new()));
    }
    match wire::pop_qkc_path_next_hop(incoming_header_qkc_mp) {
        Ok((nh, rest)) => {
            debug!(
                next_hop = nh,
                dest_final, "qkc multipath: next_hop from header_qkc_mp"
            );
            Ok((nh, rest))
        }
        Err(e) => {
            debug!(
                error = %e,
                dest_final,
                "qkc multipath decode failed; fallback routing table"
            );
            let nh = routing_fallback(dest_final).ok_or(QkcError::NoRoute(dest_final))?;
            Ok((nh, Vec::new()))
        }
    }
}

async fn forward_plaintext(
    svc: &QkcService,
    next_hop: u32,
    dest_final: u32,
    plaintext: &[u8],
    header_qkc_mp: Vec<u8>,
    header_orr_mp: Vec<u8>,
    header_dkms_mp: Vec<u8>,
) -> Result<()> {
    let out_link = svc
        .link_to(next_hop)
        .ok_or(QkcError::UnknownNeighbor(next_hop))?;
    let chunk_bytes = (out_link.cfg.key_size_bits / 8) as usize;
    let needed = num_chunks(plaintext.len(), chunk_bytes);
    let enc_keys = take_or_fetch_enc(out_link, needed).await?;
    let (ciphertext, ids) = encrypt(plaintext, &enc_keys)?;
    send_frame_to_peer(
        svc,
        next_hop,
        dest_final,
        ciphertext,
        ids,
        header_qkc_mp,
        header_orr_mp,
        header_dkms_mp,
    )
}

fn send_frame_to_peer(
    svc: &QkcService,
    next_hop: u32,
    dest_final: u32,
    ciphertext: Vec<u8>,
    key_ids: Vec<Uuid>,
    header_qkc_mp: Vec<u8>,
    header_orr_mp: Vec<u8>,
    header_dkms_mp: Vec<u8>,
) -> Result<()> {
    let out_link = svc
        .link_to(next_hop)
        .ok_or(QkcError::UnknownNeighbor(next_hop))?;
    let kind = if next_hop == dest_final {
        FRAME_RECV
    } else {
        FRAME_RELAY
    };
    let mut out = Frame::empty(kind);
    out.sender_id = svc.qkc_id();
    out.receiver_id = next_hop;
    out.dest_final = dest_final;
    out.key_size_bits = out_link.cfg.key_size_bits as u16;
    out.key_ids = key_ids.iter().map(|u| u.to_string()).collect();
    // OBJ-006/007: source routing K-Splittable. `header_qkc_mp` lleva
    // el `qkc_path` con el resto del path (tras pop del primer hop).
    // El QKC siguiente lo decodificará y popeará su next_hop. Si vacío,
    // el QKC siguiente fallback a su routing table.
    out.header_qkc_mp = header_qkc_mp;
    out.header_orr_mp = header_orr_mp;
    out.header_dkms_mp = header_dkms_mp;
    out.payload = ciphertext;

    let addr = svc
        .neighbor_peer_addr(next_hop)
        .ok_or(QkcError::UnknownNeighbor(next_hop))?;
    let ok = svc.peer_out.send(next_hop, &addr, out);
    if !ok {
        return Err(QkcError::Quditto(format!(
            "peer_out queue full for {next_hop}"
        )));
    }
    Ok(())
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

    // ── OBJ-008: tests resolve_next_hop (source routing K-Splittable) ──

    /// Caso 1 — header con path multi-hop: el QKC pop-ea el primer hop
    /// y propaga el resto en el header del frame saliente.
    #[test]
    fn resolve_next_hop_multipath_pops_first_and_propagates_rest() {
        let path = vec![2_u32, 3, 4];
        let bytes = wire::encode_qkc_path(&path);
        let (next, out_header) = resolve_next_hop(&bytes, /*dest_final*/ 4, |_| None)
            .expect("multipath debe resolver sin tocar routing fallback");
        assert_eq!(next, 2, "next_hop = primer elemento del path");
        let rest = wire::decode_qkc_path(&out_header).unwrap();
        assert_eq!(rest, vec![3_u32, 4], "rest = path[1..]");
    }

    /// Caso 2 — header con un solo hop (penúltimo QKC): pop deja el
    /// path vacío. El frame saliente lleva `header_qkc_mp` vacío para
    /// que el QKC destino caiga a su routing table normal (entrega
    /// local).
    #[test]
    fn resolve_next_hop_single_hop_leaves_empty_header() {
        let bytes = wire::encode_qkc_path(&[4_u32]);
        let (next, out_header) = resolve_next_hop(&bytes, 4, |_| None)
            .expect("single-hop multipath debe resolver");
        assert_eq!(next, 4);
        assert!(
            out_header.is_empty(),
            "tras pop del único hop, header queda vacío (convención wire)"
        );
    }

    /// Caso 3 — `header_qkc_mp` vacío: fallback al routing table del
    /// QKC. El header saliente queda vacío.
    #[test]
    fn resolve_next_hop_empty_header_falls_back_to_routing_table() {
        let (next, out_header) = resolve_next_hop(&[], /*dest_final*/ 7, |d| {
            assert_eq!(d, 7, "fallback recibe el dest_final original");
            Some(99)
        })
        .expect("fallback debe resolver");
        assert_eq!(next, 99, "next_hop viene del routing table");
        assert!(out_header.is_empty());
    }

    /// Caso 4 — `header_qkc_mp` corrupto (bytes no-msgpack): fallback
    /// silencioso al routing table. No crash, no error propagado al
    /// caller mientras el fallback exista.
    #[test]
    fn resolve_next_hop_corrupt_header_falls_back() {
        let garbage = vec![0xff_u8, 0xff, 0xff, 0xff, 0xff];
        let (next, out_header) =
            resolve_next_hop(&garbage, /*dest_final*/ 5, |_| Some(42))
                .expect("fallback debe absorber el decode failure");
        assert_eq!(next, 42, "next_hop del fallback");
        assert!(
            out_header.is_empty(),
            "header corrupto no se propaga downstream"
        );
    }

    /// Caso 5 — header vacío + routing table tampoco resuelve: error
    /// `NoRoute` propagado al caller.
    #[test]
    fn resolve_next_hop_no_route_returns_err() {
        let r = resolve_next_hop(&[], 99, |_| None);
        match r {
            Err(QkcError::NoRoute(d)) => assert_eq!(d, 99),
            other => panic!("expected NoRoute, got {other:?}"),
        }
    }

    /// Caso 6 — header corrupto + routing table tampoco resuelve:
    /// también `NoRoute` (fallback intentado, sin éxito).
    #[test]
    fn resolve_next_hop_corrupt_header_and_no_route_returns_err() {
        let garbage = vec![0xff, 0xff];
        let r = resolve_next_hop(&garbage, 5, |_| None);
        assert!(matches!(r, Err(QkcError::NoRoute(_))));
    }

    /// OBJ-011 (smoke unitario de cadena multipath): simula 4 QKCs
    /// procesando el path `[2,3,4]` en cascada. Cada QKC popea el
    /// primer hop y deja el resto en el header saliente. Tras pasar
    /// por todos, el path se agota y el último QKC cae al routing
    /// table (entrega local).
    ///
    /// Este test sustituye al smoke multi-nodo con docker-compose
    /// porque el `docker-compose.yml` del repo es single-instance
    /// (1 qkc, 1 orr, 1 sdn) y replicarlo a 4 nodos requeriría
    /// orchestación que sólo aplica en EKS (Fase F).
    #[test]
    fn smoke_chain_4_qkcs_consume_path_in_order() {
        // ORR origen encodea el path completo.
        let initial_header = wire::encode_qkc_path(&[2_u32, 3, 4]);
        assert!(!initial_header.is_empty());

        // QKC-1 (handle_local_send): pop → next=2.
        let (nh1, hdr1) = resolve_next_hop(&initial_header, 4, |_| {
            panic!("multipath debería resolver sin tocar fallback")
        })
        .unwrap();
        assert_eq!(nh1, 2);
        assert_eq!(wire::decode_qkc_path(&hdr1).unwrap(), vec![3_u32, 4]);

        // QKC-2 (handle_incoming): pop → next=3.
        let (nh2, hdr2) = resolve_next_hop(&hdr1, 4, |_| {
            panic!("multipath debería resolver sin tocar fallback")
        })
        .unwrap();
        assert_eq!(nh2, 3);
        assert_eq!(wire::decode_qkc_path(&hdr2).unwrap(), vec![4_u32]);

        // QKC-3 (handle_incoming penúltimo): pop → next=4, header vacío.
        let (nh3, hdr3) = resolve_next_hop(&hdr2, 4, |_| {
            panic!("multipath debería resolver sin tocar fallback")
        })
        .unwrap();
        assert_eq!(nh3, 4);
        assert!(hdr3.is_empty(), "tras el último pop, header limpio");

        // QKC-4 (destino final): header vacío → fallback routing table.
        // En este test el fallback retorna Some(4) simulando "soy yo, entrega local".
        let mut fallback_called = false;
        let (nh4, _) = resolve_next_hop(&hdr3, 4, |d| {
            fallback_called = true;
            assert_eq!(d, 4);
            Some(4)
        })
        .unwrap();
        assert_eq!(nh4, 4);
        assert!(fallback_called, "QKC destino debe caer al routing table");
    }
}
