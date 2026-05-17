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
//!   header_qkc_mp    ← cleartext, lo añade QKC, varía hop a hop
//! ```
//!
//! El QKC propaga `header_orr_mp` y `header_dkms_mp` byte-a-byte sin
//! parsearlos. Solo escribe/lee `header_qkc_mp`. El ORR equivalente con
//! `header_orr_mp`. El DKMS pone los metadatos de la clave que está
//! transportando en `header_dkms_mp`.
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
//!   FRAME_TYPE 1 B  = 0x01..0x20
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

/// ORR → QKC: "envía este payload a `dest_final`. Tú te encargas del
/// cifrado del payload (OTP del enlace) y del routing".
pub const FRAME_LOCAL_SEND: u8 = 0x10;
/// QKC → ORR: "te entrego este payload que llegó dirigido a este
/// nodo". `header_qkc_mp` ya está quitado (vacío) — el ORR solo ve
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
    /// Header QKC en cleartext (msgpack). Lo escribe/lee solo el QKC.
    /// Reservado para metadatos del propio QKC (priority, ttl, etc.);
    /// hoy va vacío.
    pub header_qkc_mp: Vec<u8>,
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
            sender_id: 0,
            receiver_id: 0,
            dest_final: 0,
            key_size_bits: 0,
            epoch_id: 0,
            key_ids: Vec::new(),
            header_qkc_mp: Vec::new(),
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
            + self.header_qkc_mp.len()
            + 2
            + self.header_orr_mp.len()
            + 2
            + self.header_dkms_mp.len()
            + 4
            + self.payload.len();

        let mut buf = BytesMut::with_capacity(FIXED_PREFIX + body_len);
        buf.put_slice(&MAGIC);
        buf.put_u8(self.kind);
        buf.put_u8(0);
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
        buf.put_u16_le(self.header_qkc_mp.len() as u16);
        buf.put_slice(&self.header_qkc_mp);
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

        let header_qkc_mp = read_lp16(&mut body, "header_qkc")?;
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
            kind: 0, // caller fills in
            sender_id,
            receiver_id,
            dest_final,
            key_size_bits,
            epoch_id,
            key_ids,
            header_qkc_mp,
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
    fn roundtrip_all_three_headers() {
        let f = Frame {
            kind: FRAME_RECV,
            sender_id: 1,
            receiver_id: 2,
            dest_final: 2,
            key_size_bits: 256,
            epoch_id: 0,
            key_ids: vec!["abcdefgh".into()],
            header_qkc_mp: vec![0x80], // empty msgpack map
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
        assert_eq!(f.header_qkc_mp, f2.header_qkc_mp);
        assert_eq!(f.header_orr_mp, f2.header_orr_mp);
        assert_eq!(f.header_dkms_mp, f2.header_dkms_mp);
        assert_eq!(f.payload, f2.payload);
    }

    #[test]
    fn roundtrip_empty_headers() {
        // Caso típico de FRAME_LOCAL_SEND inicial donde ni ORR ni DKMS
        // han metido aún sus headers.
        let f = Frame {
            kind: FRAME_LOCAL_SEND,
            sender_id: 0,
            receiver_id: 0,
            dest_final: 3,
            key_size_bits: 0,
            epoch_id: 0,
            key_ids: vec![],
            header_qkc_mp: vec![],
            header_orr_mp: vec![],
            header_dkms_mp: vec![],
            payload: vec![1, 2, 3, 4, 5],
        };
        let buf = f.encode();
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert!(f2.header_qkc_mp.is_empty());
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
        assert!(f2.header_qkc_mp.is_empty());
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
        buf.put_u16_le(0); // hdr_qkc len
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
            kind: FRAME_LOCAL_SEND,
            sender_id: 7,
            receiver_id: 11,
            dest_final: 11,
            key_size_bits: 0,
            epoch_id: 0xDEADBEEF,
            key_ids: vec![],
            header_qkc_mp: vec![],
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
    }

    #[test]
    fn notify_payload_round_trip() {
        let ids = vec![[1u8; 16], [2u8; 16], [3u8; 16]];
        let buf = encode_notify_payload(&ids);
        let back = decode_notify_payload(&buf).unwrap();
        assert_eq!(back, ids);
    }

    #[test]
    fn notify_payload_rejects_truncated() {
        let r = decode_notify_payload(&[1, 0, 0, 0]); // dice count=1 pero falta el UUID
        assert!(r.is_err());
    }
}
