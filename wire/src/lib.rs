//! Wire binario TCP usado por:
//!
//! * **QKC ↔ QKC** — frames `FRAME_RECV` / `FRAME_RELAY` / `FRAME_ACK`.
//! * **ORR ↔ QKC** — frames `FRAME_LOCAL_SEND` / `FRAME_LOCAL_DELIVER`
//!   (mismo wire, otra tag de kind).
//!
//! ## Modelo de capas
//!
//! Cada capa (QKC, ORR, DKMS) tiene su propio header **en claro** que
//! viaja al lado del cifrado y que nadie más toca:
//!
//! ```text
//!   payload          ← qkc_crypt{ orr_crypt{ body_dkms } }
//!   header_dkms_mp   ← cleartext, lo añade DKMS, nadie lo toca
//!   header_orr_mp    ← cleartext, lo añade ORR, nadie lo toca
//! ```
//!
//! El QKC propaga `header_orr_mp` y `header_dkms_mp` byte-a-byte sin
//! parsearlos. El ORR escribe/lee `header_orr_mp`. El DKMS pone los
//! metadatos de la clave que está transportando en `header_dkms_mp`.
//!
//! Confidencialidad del payload: la pone el OTP del enlace QKC↔QKC. Los
//! frames LOCAL viajan en claro sobre TCP de localhost; el riesgo de
//! eavesdropping en loopback no aplica al threat model.
//!
//! ## Wire format (little-endian, sin padding)
//!
//! ```text
//! Prefijo fijo (10 B):
//!   MAGIC      4 B  = b"\x51\x4B\x43\x03"   ('Q','K','C', v3)
//!   FRAME_TYPE 1 B  = 0x01..0x22
//!   RESERVED   1 B  = 0x00
//!   TOTAL_LEN  4 B  u32 LE — bytes restantes (no incluye prefijo)
//!
//! Payload variable:
//!   SENDER_ID     4 B  u32 LE
//!   RECEIVER_ID   4 B  u32 LE
//!   DEST_FINAL    4 B  u32 LE   (en RECV/LOCAL_DELIVER = RECEIVER_ID)
//!   KEY_SIZE_BITS 2 B  u16 LE   (0 = sin cifrado, p.ej. frames LOCAL)
//!   EPOCH_ID      4 B  u32 BE   (ORR Option-B forward secrecy: identifica
//!                                la época del master_secret usado por la
//!                                capa ORR de este frame; 0 = pre-rotación
//!                                / passthrough sin epoch)
//!   N_KEY_IDS     1 B  u8       (0 cuando el frame no lleva keys)
//!   KEY_ID_LEN    1 B  u8       (longitud uniforme por key_id)
//!   KEY_IDS       N_KEY_IDS * KEY_ID_LEN
//!   HDR_QKC_LEN   2 B  u16 LE
//!   HDR_QKC       HDR_QKC_LEN B   (msgpack — QKC-level metadata)
//!   HDR_ORR_LEN   2 B  u16 LE
//!   HDR_ORR       HDR_ORR_LEN B   (msgpack — ORR-level metadata)
//!   HDR_DKMS_LEN  2 B  u16 LE
//!   HDR_DKMS      HDR_DKMS_LEN B  (msgpack — DKMS-level metadata)
//!   PAYLOAD_LEN   4 B  u32 LE
//!   PAYLOAD       PAYLOAD_LEN B   (ciphertext o plaintext según kind)
//! ```
//!
//! ## Versionado
//!
//! El último byte de `MAGIC` codifica la versión del wire. `v3` (este
//! archivo) introduce el campo `EPOCH_ID` para soportar la rotación de
//! master_secret del ORR (Option B forward secrecy, audit H-3). `v2` no
//! es bit-compatible: los lectores `v3` rechazan frames `v2` con
//! `BadMagic`, y viceversa. La incompatibilidad es deliberada para que
//! deployments mezclados fallen ruidosos en lugar de misparsear.

use std::io;

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: [u8; 4] = [0x51, 0x4B, 0x43, 0x03]; // 'Q','K','C', v3

/// Frame QKC→QKC: el destino final soy yo. Payload cifrado con OTP.
pub const FRAME_RECV: u8 = 0x01;
/// Frame QKC→QKC: relay multi-hop. Payload cifrado con OTP del link
/// entrante; el QKC intermedio descifra, recifra con el link saliente,
/// y reenvía con `dest_final` intacto.
pub const FRAME_RELAY: u8 = 0x02;
/// ACK aplicación (DKMS-level). Plaintext sin OTP en el wire.
pub const FRAME_ACK: u8 = 0x03;

/// Variantes **autenticadas** de `FRAME_RECV` / `FRAME_RELAY`.
///
/// Mismo cuerpo que 0x01/0x02, pero el `PAYLOAD` termina en un trailer de
/// [`AUTH_TRAILER_LEN`] bytes: `session(8) ‖ counter(8) ‖ tag(32)`. El tag es un
/// HMAC-SHA256 calculado por `common::crypto::frame_mac` sobre **todo** el frame
/// (identidades, headers y ciphertext incluidos), con clave derivada de la raíz
/// del enlace y de la sesión.
///
/// Cierran a la vez las tres cosas que el OTP no da: integridad (XOR es
/// maleable), autenticación de origen (`sender_id` es un `u32` sin verificar) y
/// frescura (`counter` + ventana deslizante en el receptor).
///
/// Un peer viejo que no conozca estos kinds los ignora en silencio (ver
/// `peer_server`), así que el rollout va por el flag `frame_auth = off|prefer|
/// require`, igual que el del handshake.
pub const FRAME_RECV_AUTH: u8 = 0x04;
pub const FRAME_RELAY_AUTH: u8 = 0x05;

/// ORR → QKC: "envía este payload a `dest_final`. Tú te encargas del
/// cifrado del payload (OTP del enlace) y del routing".
pub const FRAME_LOCAL_SEND: u8 = 0x10;
/// QKC → ORR: "te entrego este payload que llegó dirigido a este
/// nodo". El ORR solo ve
/// `header_orr_mp`, `header_dkms_mp` y el payload.
pub const FRAME_LOCAL_DELIVER: u8 = 0x11;

/// QKC_A → QKC_B (mismo enlace): notificación de que A acaba de pedir
/// estos `key_ID`s al quditto compartido. B debe llamar a `dec_keys`
/// con esos IDs para llenar su buffer DEC.
///
/// Wire del payload:
///   COUNT  4 B  u32 LE
///   IDs    COUNT * 16 B (UUID raw)
///
/// `key_ids` y los tres headers van vacíos en este tipo de frame.
pub const FRAME_KEY_IDS_NOTIFY: u8 = 0x20;

/// QKC_A → QKC_B (enlace **PQC**): primer paso del handshake ML-KEM. El
/// iniciador (qkc_id menor) manda su clave pública ML-KEM en `payload`.
/// `key_ids` y los tres headers van vacíos.
pub const FRAME_PQC_KEM_INIT: u8 = 0x21;
/// QKC_B → QKC_A (enlace **PQC**): respuesta del handshake ML-KEM. El
/// respondedor manda el ciphertext de la encapsulación en `payload`; ambos
/// extremos quedan con el mismo secreto de 32 B. `key_ids` y los tres
/// headers van vacíos.
pub const FRAME_PQC_KEM_RESP: u8 = 0x22;

/// Variantes **autenticadas** del handshake PQC (docs/SECURITY.md §Fase 5).
/// Mismo `payload = epoch_be(4) ‖ blob` que 0x21/0x22/0x20, pero con un tag
/// HMAC-SHA256 de 32 B anexado al final (`payload ‖ tag`). El tag se calcula
/// con `common::crypto::link_mac` sobre un PSK por enlace. Un peer viejo que
/// no entienda estos kinds los ignora en silencio (ver `peer_server`), así
/// que el rollout es por el flag `pqc_auth = off|prefer|require`.
pub const FRAME_PQC_KEM_INIT_AUTH: u8 = 0x23;
pub const FRAME_PQC_KEM_RESP_AUTH: u8 = 0x24;
/// `FRAME_KEY_IDS_NOTIFY` autenticado (mismo payload ‖ tag de 32 B).
pub const FRAME_KEY_IDS_NOTIFY_AUTH: u8 = 0x25;
/// Reservados para un futuro handshake **firmado** (ML-DSA) — no usados aún.
pub const FRAME_PQC_KEM_INIT_SIGNED: u8 = 0x26;
pub const FRAME_PQC_KEM_RESP_SIGNED: u8 = 0x27;

/// Valores del campo [`Frame::grade`] (byte RESERVED del prefijo).
/// `0` = QKD-grade (todo el camino QKD; default), `1` = PQC-grade (≥1 salto PQC).
pub const GRADE_QKD: u8 = 0;
pub const GRADE_PQC: u8 = 1;

const FIXED_PREFIX: usize = 4 + 1 + 1 + 4;
const MAX_TOTAL_LEN: u32 = 64 * 1024 * 1024; // 64 MiB safety cap

#[derive(Debug, Error)]
pub enum WireError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("bad magic / version")]
    BadMagic,
    #[error("frame too large: {0} bytes")]
    TooLarge(u32),
    #[error("truncated frame: {0}")]
    Truncated(&'static str),
}

/// Frame deserializado. `kind` lo rellena el reader desde el prefijo.
#[derive(Debug, Clone)]
pub struct Frame {
    pub kind: u8,
    /// Grado de seguridad de la clave que transporta el frame, en el byte
    /// RESERVED del prefijo: `0` = QKD-grade (default), `1` = PQC-grade. El
    /// QKC lo usa para elegir la tabla de forwarding (solo-QKD vs full) y así
    /// preservar la invariante "una clave QKD-grade nunca cruza un enlace PQC".
    /// El reader lo rellena desde el prefijo (igual que `kind`).
    pub grade: u8,
    pub sender_id: u32,
    pub receiver_id: u32,
    pub dest_final: u32,
    pub key_size_bits: u16,
    /// Identifica la época del `master_secret` ORR usada para cifrar la
    /// capa más externa del onion en `payload`. Codificado en BE (u32)
    /// para ser idéntico al input HMAC de la rotación (`epoch_be`). `0`
    /// significa "sin capa onion" (passthrough modo 0) o "pre-rotación"
    /// (compatibilidad con código legado antes del audit H-3).
    pub epoch_id: u32,
    pub key_ids: Vec<String>,
    /// Header ORR en cleartext (msgpack). Lo escribe el ORR origen y lo
    /// reescribe cada ORR que pela una capa onion. El QKC NO lo toca:
    /// lo propaga byte-a-byte.
    pub header_orr_mp: Vec<u8>,
    /// Header DKMS en cleartext (msgpack). Lo escribe el DKMS origen y
    /// solo lo lee el DKMS destino. Ni ORR ni QKC lo tocan: lo propagan
    /// byte-a-byte. Lleva los metadatos de la clave QKD que viaja en el
    /// payload (key_id, sae_origin/destination, key_size_bits, etc.).
    pub header_dkms_mp: Vec<u8>,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Construye un frame "vacío" útil para llenarlo campo a campo.
    pub fn empty(kind: u8) -> Self {
        Self {
            kind,
            grade: 0,
            sender_id: 0,
            receiver_id: 0,
            dest_final: 0,
            key_size_bits: 0,
            epoch_id: 0,
            key_ids: Vec::new(),
            header_orr_mp: Vec::new(),
            header_dkms_mp: Vec::new(),
            payload: Vec::new(),
        }
    }

    /// Serializa a bytes listos para escribir al socket.
    pub fn encode(&self) -> BytesMut {
        // KEY_ID_LEN uniforme: si hay keys, todos los ids deben tener la
        // misma longitud.
        let key_id_len = self.key_ids.first().map_or(0, |s| s.len()) as u8;
        for k in &self.key_ids {
            debug_assert_eq!(k.len(), key_id_len as usize, "non-uniform key_id length");
        }

        let body_len = 4
            + 4
            + 4
            + 2
            + 4
            + 1
            + 1
            + (key_id_len as usize) * self.key_ids.len()
            + 2
            + self.header_orr_mp.len()
            + 2
            + self.header_dkms_mp.len()
            + 4
            + self.payload.len();

        let mut buf = BytesMut::with_capacity(FIXED_PREFIX + body_len);
        buf.put_slice(&MAGIC);
        buf.put_u8(self.kind);
        buf.put_u8(self.grade); // RESERVED byte now carries the key grade
        buf.put_u32_le(body_len as u32);

        buf.put_u32_le(self.sender_id);
        buf.put_u32_le(self.receiver_id);
        buf.put_u32_le(self.dest_final);
        buf.put_u16_le(self.key_size_bits);
        buf.put_u32(self.epoch_id); // BE on wire — emparejado con HMAC epoch_be
        buf.put_u8(self.key_ids.len() as u8);
        buf.put_u8(key_id_len);
        for k in &self.key_ids {
            buf.put_slice(k.as_bytes());
        }
        buf.put_u16_le(self.header_orr_mp.len() as u16);
        buf.put_slice(&self.header_orr_mp);
        buf.put_u16_le(self.header_dkms_mp.len() as u16);
        buf.put_slice(&self.header_dkms_mp);
        buf.put_u32_le(self.payload.len() as u32);
        buf.put_slice(&self.payload);
        buf
    }

    fn decode_body(mut body: &[u8]) -> Result<Frame, WireError> {
        if body.remaining() < 4 + 4 + 4 + 2 + 4 + 1 + 1 {
            return Err(WireError::Truncated("header"));
        }
        let sender_id = body.get_u32_le();
        let receiver_id = body.get_u32_le();
        let dest_final = body.get_u32_le();
        let key_size_bits = body.get_u16_le();
        let epoch_id = body.get_u32(); // BE on wire
        let n_key_ids = body.get_u8() as usize;
        let key_id_len = body.get_u8() as usize;

        if body.remaining() < n_key_ids * key_id_len + 2 {
            return Err(WireError::Truncated("key_ids"));
        }
        let mut key_ids = Vec::with_capacity(n_key_ids);
        for _ in 0..n_key_ids {
            let mut buf = vec![0u8; key_id_len];
            body.copy_to_slice(&mut buf);
            key_ids.push(String::from_utf8(buf).map_err(|_| WireError::Truncated("key_id utf-8"))?);
        }

        let header_orr_mp = read_lp16(&mut body, "header_orr")?;
        let header_dkms_mp = read_lp16(&mut body, "header_dkms")?;

        if body.remaining() < 4 {
            return Err(WireError::Truncated("payload_len"));
        }
        let payload_len = body.get_u32_le() as usize;
        if body.remaining() < payload_len {
            return Err(WireError::Truncated("payload"));
        }
        let payload = body[..payload_len].to_vec();

        Ok(Frame {
            kind: 0,  // caller fills in (from prefix)
            grade: 0, // caller fills in (from prefix RESERVED byte)
            sender_id,
            receiver_id,
            dest_final,
            key_size_bits,
            epoch_id,
            key_ids,
            header_orr_mp,
            header_dkms_mp,
            payload,
        })
    }
}

/// Lee un campo length-prefixed u16 LE.
fn read_lp16(body: &mut &[u8], what: &'static str) -> Result<Vec<u8>, WireError> {
    if body.remaining() < 2 {
        return Err(WireError::Truncated(what));
    }
    let len = body.get_u16_le() as usize;
    if body.remaining() < len {
        return Err(WireError::Truncated(what));
    }
    let out = body[..len].to_vec();
    body.advance(len);
    Ok(out)
}

/// Lee un frame completo desde cualquier `AsyncRead` (TcpStream,
/// OwnedReadHalf, BufReader, etc.).
pub async fn read_frame<R: AsyncRead + Unpin>(stream: &mut R) -> Result<Frame, WireError> {
    let mut prefix = [0u8; FIXED_PREFIX];
    stream.read_exact(&mut prefix).await?;
    if prefix[..4] != MAGIC {
        return Err(WireError::BadMagic);
    }
    let kind = prefix[4];
    let total_len = u32::from_le_bytes([prefix[6], prefix[7], prefix[8], prefix[9]]);
    if total_len > MAX_TOTAL_LEN {
        return Err(WireError::TooLarge(total_len));
    }
    let mut body = vec![0u8; total_len as usize];
    stream.read_exact(&mut body).await?;
    let mut f = Frame::decode_body(&body)?;
    f.kind = kind;
    f.grade = prefix[5]; // RESERVED byte carries the key grade
    Ok(f)
}

/// Escribe un frame a cualquier `AsyncWrite` (TcpStream,
/// OwnedWriteHalf, BufWriter, etc.).
pub async fn write_frame<W: AsyncWrite + Unpin>(
    stream: &mut W,
    frame: &Frame,
) -> Result<(), WireError> {
    let buf = frame.encode();
    stream.write_all(&buf).await?;
    Ok(())
}

// ─── helpers para FRAME_RECV_AUTH / FRAME_RELAY_AUTH ───────────────

/// Bytes que el trailer de autenticación añade al final del `PAYLOAD`:
/// `session(8 B LE) ‖ counter(8 B LE) ‖ tag(32 B)`.
pub const AUTH_TRAILER_LEN: usize = 8 + 8 + 32;

/// `true` si el kind lleva trailer de autenticación.
pub fn is_auth_kind(kind: u8) -> bool {
    matches!(
        kind,
        FRAME_RECV_AUTH | FRAME_RELAY_AUTH | FRAME_KEY_IDS_NOTIFY_AUTH
    )
}

/// Variante autenticada de un kind, si la tiene.
///
/// El NOTIFY entra aquí a propósito: comparte el mismo trailer que los frames de
/// datos, y así comparte también el contador y la ventana anti-replay. Antes
/// llevaba un HMAC propio sin frescura, y un NOTIFY capturado se reinyectaba sin
/// problema.
pub fn auth_kind_for(kind: u8) -> Option<u8> {
    match kind {
        FRAME_RECV => Some(FRAME_RECV_AUTH),
        FRAME_RELAY => Some(FRAME_RELAY_AUTH),
        FRAME_KEY_IDS_NOTIFY => Some(FRAME_KEY_IDS_NOTIFY_AUTH),
        _ => None,
    }
}

/// Kind base de una variante autenticada — lo que el resto del QKC espera ver
/// una vez comprobado y quitado el trailer.
pub fn base_kind_of(kind: u8) -> u8 {
    match kind {
        FRAME_RECV_AUTH => FRAME_RECV,
        FRAME_RELAY_AUTH => FRAME_RELAY,
        FRAME_KEY_IDS_NOTIFY_AUTH => FRAME_KEY_IDS_NOTIFY,
        other => other,
    }
}

/// Añade el trailer de autenticación al final del payload.
pub fn append_auth_trailer(payload: &mut Vec<u8>, session: u64, counter: u64, tag: &[u8; 32]) {
    payload.reserve(AUTH_TRAILER_LEN);
    payload.extend_from_slice(&session.to_le_bytes());
    payload.extend_from_slice(&counter.to_le_bytes());
    payload.extend_from_slice(tag);
}

/// Separa el payload de un frame autenticado en `(body, session, counter, tag)`.
///
/// `body` es el ciphertext sin trailer: es lo que hay que descifrar y lo que
/// entra en el MAC (el tag no puede cubrirse a sí mismo).
pub fn split_auth_trailer(payload: &[u8]) -> Result<(&[u8], u64, u64, &[u8]), WireError> {
    if payload.len() < AUTH_TRAILER_LEN {
        return Err(WireError::Truncated("auth trailer"));
    }
    let cut = payload.len() - AUTH_TRAILER_LEN;
    let (body, tr) = payload.split_at(cut);
    let session = u64::from_le_bytes(tr[0..8].try_into().expect("8 bytes"));
    let counter = u64::from_le_bytes(tr[8..16].try_into().expect("8 bytes"));
    Ok((body, session, counter, &tr[16..]))
}

// ─── helpers para FRAME_KEY_IDS_NOTIFY ─────────────────────────────

/// Serializa una lista de UUIDs en el formato del payload de
/// `FRAME_KEY_IDS_NOTIFY`: `count u32 LE` + `count × 16 B`.
pub fn encode_notify_payload(ids: &[[u8; 16]]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + ids.len() * 16);
    buf.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        buf.extend_from_slice(id);
    }
    buf
}

/// Deserializa el payload de un `FRAME_KEY_IDS_NOTIFY`. Devuelve los
/// UUIDs como `[u8; 16]` (raw bytes — el caller decide si los pasa a
/// `Uuid::from_bytes`).
pub fn decode_notify_payload(buf: &[u8]) -> Result<Vec<[u8; 16]>, WireError> {
    if buf.len() < 4 {
        return Err(WireError::Truncated("notify count"));
    }
    let count = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    let expected = 4 + count * 16;
    if buf.len() < expected {
        return Err(WireError::Truncated("notify ids"));
    }
    let mut out = Vec::with_capacity(count);
    let mut cur = 4;
    for _ in 0..count {
        let mut id = [0u8; 16];
        id.copy_from_slice(&buf[cur..cur + 16]);
        out.push(id);
        cur += 16;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_rides_reserved_prefix_byte() {
        // Default grade is 0 (QKD-grade).
        assert_eq!(Frame::empty(FRAME_RELAY).grade, 0);
        assert_eq!(Frame::empty(FRAME_RELAY).encode()[5], 0);
        // A PQC-grade frame writes 1 at the RESERVED prefix byte (index 5),
        // which `read_frame` reads back into `Frame::grade`.
        let mut f = Frame::empty(FRAME_RELAY);
        f.grade = 1;
        f.payload = vec![1, 2, 3];
        let bytes = f.encode();
        assert_eq!(bytes[5], 1, "grade must ride the RESERVED prefix byte");
    }

    #[test]
    fn roundtrip_all_three_headers() {
        let f = Frame {
            grade: 0,
            kind: FRAME_RECV,
            sender_id: 1,
            receiver_id: 2,
            dest_final: 2,
            key_size_bits: 256,
            epoch_id: 0,
            key_ids: vec!["abcdefgh".into()],
            header_orr_mp: vec![0x81, 0xa4, 0x66, 0x72, 0x6f, 0x6d, 0xa1, 0x41], // {"from":"A"}
            header_dkms_mp: vec![0x81, 0xa2, 0x69, 0x64, 0xa3, 0x6b, 0x33, 0x37], // {"id":"k37"}
            payload: vec![0xde, 0xad, 0xbe, 0xef],
        };
        let buf = f.encode();
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert_eq!(f.sender_id, f2.sender_id);
        assert_eq!(f.receiver_id, f2.receiver_id);
        assert_eq!(f.dest_final, f2.dest_final);
        assert_eq!(f.key_size_bits, f2.key_size_bits);
        assert_eq!(f.epoch_id, f2.epoch_id);
        assert_eq!(f.key_ids, f2.key_ids);
        assert_eq!(f.header_orr_mp, f2.header_orr_mp);
        assert_eq!(f.header_dkms_mp, f2.header_dkms_mp);
        assert_eq!(f.payload, f2.payload);
    }

    #[test]
    fn roundtrip_empty_headers() {
        // Caso típico de FRAME_LOCAL_SEND inicial donde ni ORR ni DKMS
        // han metido aún sus headers.
        let f = Frame {
            grade: 0,
            kind: FRAME_LOCAL_SEND,
            sender_id: 0,
            receiver_id: 0,
            dest_final: 3,
            key_size_bits: 0,
            epoch_id: 0,
            key_ids: vec![],
            header_orr_mp: vec![],
            header_dkms_mp: vec![],
            payload: vec![1, 2, 3, 4, 5],
        };
        let buf = f.encode();
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert!(f2.header_orr_mp.is_empty());
        assert!(f2.header_dkms_mp.is_empty());
        assert_eq!(f2.epoch_id, 0);
        assert_eq!(f2.payload, f.payload);
    }

    #[test]
    fn empty_frame_round_trips() {
        let f = Frame::empty(FRAME_RECV);
        let buf = f.encode();
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert_eq!(f2.sender_id, 0);
        assert_eq!(f2.payload.len(), 0);
        assert_eq!(f2.epoch_id, 0);
        assert!(f2.header_orr_mp.is_empty());
        assert!(f2.header_dkms_mp.is_empty());
    }

    #[test]
    fn truncated_dkms_header_errors() {
        // Construye un buffer válido hasta header_orr_mp y trunca antes
        // de leer header_dkms_mp.
        let mut buf = bytes::BytesMut::new();
        buf.put_u32_le(0); // sender
        buf.put_u32_le(0); // receiver
        buf.put_u32_le(0); // dest_final
        buf.put_u16_le(0); // key_size_bits
        buf.put_u32(0); // epoch_id (BE)
        buf.put_u8(0); // n_key_ids
        buf.put_u8(0); // key_id_len
        buf.put_u16_le(0); // hdr_orr len
                           // Cortamos antes de hdr_dkms len → Truncated.
        let err = Frame::decode_body(&buf).unwrap_err();
        assert!(matches!(err, WireError::Truncated(_)));
    }

    #[test]
    fn epoch_id_nonzero_round_trip() {
        // OBJ-003: roundtrip con epoch_id distinto de cero verifica que
        // el campo se escribe y se lee en BE correctamente.
        let f = Frame {
            grade: 0,
            kind: FRAME_LOCAL_SEND,
            sender_id: 7,
            receiver_id: 11,
            dest_final: 11,
            key_size_bits: 0,
            epoch_id: 0xDEADBEEF,
            key_ids: vec![],
            header_orr_mp: vec![],
            header_dkms_mp: vec![],
            payload: vec![0xAA, 0xBB],
        };
        let buf = f.encode();
        // El epoch_id debe aparecer literalmente como bytes BE en el
        // buffer codificado, justo después de key_size_bits.
        // Offset: FIXED_PREFIX (10) + sender(4) + receiver(4) +
        //          dest_final(4) + key_size_bits(2) = 24.
        assert_eq!(&buf[24..28], &0xDEADBEEFu32.to_be_bytes());
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert_eq!(f2.epoch_id, 0xDEADBEEF);
        assert_eq!(f2.payload, vec![0xAA, 0xBB]);
    }

    #[test]
    fn truncated_before_epoch_id_errors() {
        // OBJ-003: un body que se corta antes de poder leer epoch_id
        // (faltan los 4 B de epoch_id u32) devuelve Truncated.
        let mut buf = bytes::BytesMut::new();
        buf.put_u32_le(0); // sender
        buf.put_u32_le(0); // receiver
        buf.put_u32_le(0); // dest_final
        buf.put_u16_le(0); // key_size_bits
                           // Cortamos: faltan los 4 B de epoch_id (más n_key_ids+key_id_len).
        let err = Frame::decode_body(&buf).unwrap_err();
        assert!(matches!(err, WireError::Truncated(_)));
    }

    #[test]
    fn legacy_v2_magic_rejected() {
        // Versionado v3 vs v2: un prefix con MAGIC v2 (último byte 0x02)
        // debe ser rechazado por `read_frame` con BadMagic.
        let v2_prefix: [u8; FIXED_PREFIX] = [
            0x51, 0x4B, 0x43, 0x02, // MAGIC v2
            FRAME_RECV, 0x00, // kind, reserved
            0, 0, 0, 0, // total_len (irrelevante aquí)
        ];
        let mut cursor: &[u8] = &v2_prefix;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(read_frame(&mut cursor)).unwrap_err();
        assert!(matches!(err, WireError::BadMagic));
    }

    #[test]
    fn local_kinds_have_distinct_values() {
        assert_ne!(FRAME_RECV, FRAME_LOCAL_SEND);
        assert_ne!(FRAME_RELAY, FRAME_LOCAL_SEND);
        assert_ne!(FRAME_LOCAL_SEND, FRAME_LOCAL_DELIVER);
        assert_ne!(FRAME_KEY_IDS_NOTIFY, FRAME_RECV);
        assert_ne!(FRAME_KEY_IDS_NOTIFY, FRAME_LOCAL_SEND);
        // PQC handshake kinds distintos de todo lo demás y entre sí.
        for k in [
            FRAME_RECV,
            FRAME_RELAY,
            FRAME_ACK,
            FRAME_LOCAL_SEND,
            FRAME_LOCAL_DELIVER,
            FRAME_KEY_IDS_NOTIFY,
        ] {
            assert_ne!(FRAME_PQC_KEM_INIT, k);
            assert_ne!(FRAME_PQC_KEM_RESP, k);
        }
        assert_ne!(FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_RESP);
    }

    #[test]
    fn pqc_kem_frame_round_trips_large_payload() {
        // Una pubkey ML-KEM-768 son 1184 B; debe sobrevivir encode/decode.
        let mut f = Frame::empty(FRAME_PQC_KEM_INIT);
        f.sender_id = 7;
        f.receiver_id = 9;
        f.dest_final = 9;
        f.payload = vec![0xABu8; 1184];
        let bytes = f.encode();
        let back = Frame::decode_body(&bytes[FIXED_PREFIX..]).unwrap();
        assert_eq!(back.sender_id, 7);
        assert_eq!(back.payload.len(), 1184);
        assert_eq!(back.payload, f.payload);
    }

    #[test]
    fn notify_payload_round_trip() {
        let ids = vec![[1u8; 16], [2u8; 16], [3u8; 16]];
        let buf = encode_notify_payload(&ids);
        let back = decode_notify_payload(&buf).unwrap();
        assert_eq!(back, ids);
    }

    #[test]
    fn auth_trailer_round_trip() {
        let mut payload = b"ciphertext".to_vec();
        let tag = [0xABu8; 32];
        append_auth_trailer(&mut payload, 0xDEAD_BEEF_CAFE_0001, 42, &tag);
        assert_eq!(payload.len(), 10 + AUTH_TRAILER_LEN);
        let (body, session, counter, t) = split_auth_trailer(&payload).unwrap();
        assert_eq!(body, b"ciphertext");
        assert_eq!(session, 0xDEAD_BEEF_CAFE_0001);
        assert_eq!(counter, 42);
        assert_eq!(t, &tag);
    }

    #[test]
    fn auth_trailer_round_trips_through_a_frame() {
        // El trailer viaja dentro de PAYLOAD, así que tiene que sobrevivir al
        // encode/decode del frame sin que nadie lo trate distinto.
        let mut f = Frame::empty(FRAME_RELAY_AUTH);
        f.sender_id = 3;
        f.payload = b"ct".to_vec();
        append_auth_trailer(&mut f.payload, 9, 1, &[0x5Au8; 32]);
        let buf = f.encode();
        let back = Frame::decode_body(&buf[FIXED_PREFIX..]).unwrap();
        let (body, session, counter, tag) = split_auth_trailer(&back.payload).unwrap();
        assert_eq!(body, b"ct");
        assert_eq!((session, counter), (9, 1));
        assert_eq!(tag, &[0x5Au8; 32]);
    }

    #[test]
    fn auth_trailer_rejects_short_payload() {
        assert!(split_auth_trailer(&[0u8; AUTH_TRAILER_LEN - 1]).is_err());
        // Un payload de exactamente el tamaño del trailer es válido con body vacío.
        let (body, _, _, _) = split_auth_trailer(&[0u8; AUTH_TRAILER_LEN]).unwrap();
        assert!(body.is_empty());
    }

    #[test]
    fn auth_kind_mapping_is_total_and_distinct() {
        assert_eq!(auth_kind_for(FRAME_RECV), Some(FRAME_RECV_AUTH));
        assert_eq!(auth_kind_for(FRAME_RELAY), Some(FRAME_RELAY_AUTH));
        assert_eq!(
            auth_kind_for(FRAME_KEY_IDS_NOTIFY),
            Some(FRAME_KEY_IDS_NOTIFY_AUTH)
        );
        assert_eq!(auth_kind_for(FRAME_ACK), None);
        assert_eq!(base_kind_of(FRAME_RECV_AUTH), FRAME_RECV);
        assert_eq!(base_kind_of(FRAME_RELAY_AUTH), FRAME_RELAY);
        assert_eq!(
            base_kind_of(FRAME_KEY_IDS_NOTIFY_AUTH),
            FRAME_KEY_IDS_NOTIFY
        );
        // base_kind_of es identidad en todo lo demás.
        assert_eq!(base_kind_of(FRAME_LOCAL_SEND), FRAME_LOCAL_SEND);
        assert!(is_auth_kind(FRAME_RECV_AUTH) && is_auth_kind(FRAME_RELAY_AUTH));
        assert!(is_auth_kind(FRAME_KEY_IDS_NOTIFY_AUTH));
        assert!(!is_auth_kind(FRAME_RECV) && !is_auth_kind(FRAME_KEY_IDS_NOTIFY));
        // Los kinds nuevos no chocan con ninguno de los ya asignados.
        for k in [
            FRAME_RECV,
            FRAME_RELAY,
            FRAME_ACK,
            FRAME_LOCAL_SEND,
            FRAME_LOCAL_DELIVER,
            FRAME_KEY_IDS_NOTIFY,
            FRAME_PQC_KEM_INIT,
            FRAME_PQC_KEM_RESP,
            FRAME_PQC_KEM_INIT_AUTH,
            FRAME_PQC_KEM_RESP_AUTH,
            FRAME_KEY_IDS_NOTIFY_AUTH,
            FRAME_ACK,
            FRAME_PQC_KEM_INIT_SIGNED,
            FRAME_PQC_KEM_RESP_SIGNED,
        ] {
            assert_ne!(FRAME_RECV_AUTH, k);
            assert_ne!(FRAME_RELAY_AUTH, k);
        }
    }

    #[test]
    fn notify_payload_rejects_truncated() {
        let r = decode_notify_payload(&[1, 0, 0, 0]); // dice count=1 pero falta el UUID
        assert!(r.is_err());
    }
}
