//! Wire binario TCP usado por:
//!
//! * **QKC ↔ QKC** — frames `FRAME_RECV` / `FRAME_RELAY` / `FRAME_ACK`.
//! * **ORR ↔ QKC** — frames `FRAME_LOCAL_SEND` / `FRAME_LOCAL_DELIVER`
//!   (mismo wire, otra tag de kind).
//!
//! Confidencialidad del payload: la pone el OTP del enlace QKC↔QKC. Los
//! frames LOCAL viajan en claro sobre TCP de localhost; el riesgo de
//! eavesdropping en loopback no aplica al threat model.
//!
//! Wire format (little-endian, sin padding):
//!
//! ```text
//! Prefijo fijo (10 B):
//!   MAGIC      4 B  = b"\x51\x4B\x43\x01"   ('Q','K','C', v1)
//!   FRAME_TYPE 1 B  = 0x01..0x11
//!   RESERVED   1 B  = 0x00
//!   TOTAL_LEN  4 B  u32 LE — bytes restantes (no incluye prefijo)
//!
//! Payload variable:
//!   SENDER_ID     4 B  u32 LE
//!   RECEIVER_ID   4 B  u32 LE
//!   DEST_FINAL    4 B  u32 LE   (en RECV/LOCAL_DELIVER = RECEIVER_ID)
//!   KEY_SIZE_BITS 2 B  u16 LE   (0 = sin cifrado, p.ej. frames LOCAL)
//!   N_KEY_IDS     1 B  u8       (0 cuando el frame no lleva keys)
//!   KEY_ID_LEN    1 B  u8       (longitud uniforme por key_id)
//!   KEY_IDS       N_KEY_IDS * KEY_ID_LEN
//!   HEADER_LEN    2 B  u16 LE
//!   HEADER        HEADER_LEN B  (msgpack(map))
//!   PAYLOAD_LEN   4 B  u32 LE
//!   PAYLOAD       PAYLOAD_LEN B (ciphertext o plaintext según kind)
//! ```

use std::io;

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAGIC: [u8; 4] = [0x51, 0x4B, 0x43, 0x01]; // 'Q','K','C',1

/// Frame QKC→QKC: el destino final soy yo. Payload cifrado con OTP.
pub const FRAME_RECV: u8 = 0x01;
/// Frame QKC→QKC: relay multi-hop. Payload cifrado con OTP del link
/// entrante; el QKC intermedio descifra, recifra con el link saliente,
/// y reenvía con `dest_final` intacto.
pub const FRAME_RELAY: u8 = 0x02;
/// ACK aplicación (DKMS-level). Plaintext sin OTP en el wire.
pub const FRAME_ACK: u8 = 0x03;

/// ORR → QKC: "envía este payload (plaintext) a `dest_final`. Tú te
/// encargas del cifrado y del routing".
pub const FRAME_LOCAL_SEND: u8 = 0x10;
/// QKC → ORR: "te entrego este plaintext que llegó dirigido a este
/// nodo".
pub const FRAME_LOCAL_DELIVER: u8 = 0x11;

/// QKC_A → QKC_B (mismo enlace): notificación de que A acaba de pedir
/// estos `key_ID`s al quditto compartido. B debe llamar a `dec_keys`
/// con esos IDs para llenar su buffer DEC.
///
/// Wire del payload:
///   COUNT  4 B  u32 LE
///   IDs    COUNT * 16 B (UUID raw)
///
/// `key_ids` y `header_mp` van vacíos en este tipo de frame.
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
    pub kind:          u8,
    pub sender_id:     u32,
    pub receiver_id:   u32,
    pub dest_final:    u32,
    pub key_size_bits: u16,
    pub key_ids:       Vec<String>,
    pub header_mp:     Vec<u8>,
    pub payload:       Vec<u8>,
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
            key_ids: Vec::new(),
            header_mp: Vec::new(),
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

        let body_len = 4 + 4 + 4 + 2 + 1 + 1
            + (key_id_len as usize) * self.key_ids.len()
            + 2 + self.header_mp.len()
            + 4 + self.payload.len();

        let mut buf = BytesMut::with_capacity(FIXED_PREFIX + body_len);
        buf.put_slice(&MAGIC);
        buf.put_u8(self.kind);
        buf.put_u8(0);
        buf.put_u32_le(body_len as u32);

        buf.put_u32_le(self.sender_id);
        buf.put_u32_le(self.receiver_id);
        buf.put_u32_le(self.dest_final);
        buf.put_u16_le(self.key_size_bits);
        buf.put_u8(self.key_ids.len() as u8);
        buf.put_u8(key_id_len);
        for k in &self.key_ids {
            buf.put_slice(k.as_bytes());
        }
        buf.put_u16_le(self.header_mp.len() as u16);
        buf.put_slice(&self.header_mp);
        buf.put_u32_le(self.payload.len() as u32);
        buf.put_slice(&self.payload);
        buf
    }

    fn decode_body(mut body: &[u8]) -> Result<Frame, WireError> {
        if body.remaining() < 4 + 4 + 4 + 2 + 1 + 1 {
            return Err(WireError::Truncated("header"));
        }
        let sender_id    = body.get_u32_le();
        let receiver_id  = body.get_u32_le();
        let dest_final   = body.get_u32_le();
        let key_size_bits = body.get_u16_le();
        let n_key_ids    = body.get_u8() as usize;
        let key_id_len   = body.get_u8() as usize;

        if body.remaining() < n_key_ids * key_id_len + 2 {
            return Err(WireError::Truncated("key_ids"));
        }
        let mut key_ids = Vec::with_capacity(n_key_ids);
        for _ in 0..n_key_ids {
            let mut buf = vec![0u8; key_id_len];
            body.copy_to_slice(&mut buf);
            key_ids
                .push(String::from_utf8(buf).map_err(|_| WireError::Truncated("key_id utf-8"))?);
        }
        let header_len = body.get_u16_le() as usize;
        if body.remaining() < header_len + 4 {
            return Err(WireError::Truncated("header_mp"));
        }
        let header_mp = body[..header_len].to_vec();
        body.advance(header_len);
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
            key_ids,
            header_mp,
            payload,
        })
    }
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
    fn roundtrip_empty_payload() {
        let f = Frame {
            kind: FRAME_RECV,
            sender_id: 1,
            receiver_id: 2,
            dest_final: 2,
            key_size_bits: 256,
            key_ids: vec!["abcdefgh".into()],
            header_mp: vec![0x80], // empty msgpack map
            payload: vec![],
        };
        let buf = f.encode();
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert_eq!(f.sender_id,     f2.sender_id);
        assert_eq!(f.receiver_id,   f2.receiver_id);
        assert_eq!(f.dest_final,    f2.dest_final);
        assert_eq!(f.key_size_bits, f2.key_size_bits);
        assert_eq!(f.key_ids,       f2.key_ids);
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
