//! Binary TCP wire for the QKC↔QKC hot path.
//!
//! This replaces the Python `QKC/socket_transport.py` and keeps the wire
//! format compatible end-to-end. Confidentiality of the payload is provided
//! by the OTP/quditto layer; integrity/auth is assumed covered by MACsec or
//! a PQC equivalent at L2. The framing here only validates magic+version so
//! we fail loudly on desync in dev.
//!
//! Wire format (little-endian, no padding):
//!
//! ```text
//! Fixed prefix (10 B):
//!   MAGIC      4 B  = b"\x51\x4B\x43\x01"   ('Q','K','C', v1)
//!   FRAME_TYPE 1 B  = 0x01 RECV | 0x02 RELAY
//!   RESERVED   1 B  = 0x00
//!   TOTAL_LEN  4 B  u32 LE — bytes remaining (excludes this prefix)
//!
//! Variable payload:
//!   SENDER_ID     4 B  u32 LE
//!   RECEIVER_ID   4 B  u32 LE
//!   DEST_FINAL    4 B  u32 LE   (== RECEIVER_ID when FRAME_RECV)
//!   KEY_SIZE_BITS 2 B  u16 LE   (0 = unset)
//!   N_KEY_IDS     1 B  u8
//!   KEY_ID_LEN    1 B  u8       (uniform ASCII length per key_id)
//!   KEY_IDS       N_KEY_IDS * KEY_ID_LEN
//!   HEADER_LEN    2 B  u16 LE
//!   HEADER        HEADER_LEN B  (msgpack(map))
//!   PAYLOAD_LEN   4 B  u32 LE
//!   PAYLOAD       PAYLOAD_LEN B (raw ciphertext, no base64)
//! ```
//!
//! Compared to HTTP/JSON+base64 the wire is 35-45% smaller and we skip
//! JSON+pydantic+base64 entirely.

use std::io;

use bytes::{Buf, BufMut, BytesMut};
use thiserror::Error;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

pub const MAGIC: [u8; 4] = [0x51, 0x4B, 0x43, 0x01]; // 'Q','K','C',1
pub const FRAME_RECV: u8 = 0x01;
pub const FRAME_RELAY: u8 = 0x02;

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
    pub fn encode(&self) -> BytesMut {
        // Compute KEY_ID_LEN as the uniform length of all key_ids; require
        // they are all the same length (callers should pre-pad if needed).
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

    pub fn decode_body(mut body: &[u8]) -> Result<Frame, WireError> {
        if body.remaining() < 4 + 4 + 4 + 2 + 1 + 1 {
            return Err(WireError::Truncated("header"));
        }
        let sender_id   = body.get_u32_le();
        let receiver_id = body.get_u32_le();
        let dest_final  = body.get_u32_le();
        let key_size_bits = body.get_u16_le();
        let n_key_ids = body.get_u8() as usize;
        let key_id_len = body.get_u8() as usize;

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
            kind: 0, // caller fills in from the prefix
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

/// Read one frame from the wire (prefix + body).
pub async fn read_frame(stream: &mut TcpStream) -> Result<Frame, WireError> {
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

/// Write a frame to the wire.
pub async fn write_frame(stream: &mut TcpStream, frame: &Frame) -> Result<(), WireError> {
    let buf = frame.encode();
    stream.write_all(&buf).await?;
    Ok(())
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
        // Strip prefix and decode.
        let (_pref, body) = buf.split_at(FIXED_PREFIX);
        let f2 = Frame::decode_body(body).unwrap();
        assert_eq!(f.sender_id,     f2.sender_id);
        assert_eq!(f.receiver_id,   f2.receiver_id);
        assert_eq!(f.dest_final,    f2.dest_final);
        assert_eq!(f.key_size_bits, f2.key_size_bits);
        assert_eq!(f.key_ids,       f2.key_ids);
    }
}
