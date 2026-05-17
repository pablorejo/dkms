//! Wire binario alternativo para los endpoints `enc_keys` / `dec_keys`.
//!
//! No es parte del estándar ETSI 014 (que define JSON+base64), pero el
//! estándar permite negociar el formato vía cabecera `Accept`. Cuando
//! cliente y servidor lo soportan, ahorramos:
//!
//! * El base64 (33% más bytes en el wire + CPU encoding/decoding).
//! * El parsing JSON (un orden de magnitud más lento que un layout
//!   posicional binario para datos uniformes como una lista de claves).
//!
//! Negociación:
//!
//! * Cliente: pone `Accept: application/octet-stream` en GET, y/o
//!   `Content-Type: application/octet-stream` en POST.
//! * Servidor: si lo ve, responde el binario; si no, responde ETSI 014
//!   JSON estándar (back-compat total con clientes ortodoxos).
//!
//! Wire para **respuesta de claves** (enc_keys, dec_keys):
//!
//! ```text
//!   MAGIC          4 B  = b"QKDB"
//!   VERSION        1 B  = 0x01
//!   KEY_SIZE_BITS  2 B  u16 LE
//!   KEY_COUNT      4 B  u32 LE
//!   por cada clave:
//!     KEY_ID       16 B  (UUID raw bytes)
//!     MATERIAL     KEY_SIZE_BITS/8 B
//! ```
//!
//! Wire para **body de dec_keys** (POST):
//!
//! ```text
//!   MAGIC          4 B  = b"QKDB"
//!   VERSION        1 B  = 0x01
//!   ID_COUNT       4 B  u32 LE
//!   por cada id:
//!     KEY_ID       16 B  (UUID raw bytes)
//! ```

use thiserror::Error;
use uuid::Uuid;

/// Content type usado en `Accept` / `Content-Type` para negociar este
/// wire binario.
pub const CONTENT_TYPE: &str = "application/octet-stream";

const MAGIC: [u8; 4] = *b"QKDB";
const VERSION: u8 = 0x01;

const KEYS_HDR_LEN: usize = 4 + 1 + 2 + 4; // magic + ver + bits + count
const IDS_HDR_LEN: usize = 4 + 1 + 4; // magic + ver + count

#[derive(Debug, Error)]
pub enum BinaryError {
    #[error("bad magic (expected QKDB)")]
    BadMagic,
    #[error("bad version: 0x{0:02x}")]
    BadVersion(u8),
    #[error("truncated wire")]
    Truncated,
    #[error("bad uuid")]
    BadUuid,
}

// ───────────────────── pack ─────────────────────

/// Serializa una lista de `(key_id, material)` al wire binario.
/// Todas las claves deben tener el mismo tamaño (`key_size_bits`).
pub fn pack_keys(keys: &[(Uuid, &[u8])], key_size_bits: u16) -> Vec<u8> {
    let key_bytes = (key_size_bits as usize).div_ceil(8);
    let total = KEYS_HDR_LEN + keys.len() * (16 + key_bytes);
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&key_size_bits.to_le_bytes());
    out.extend_from_slice(&(keys.len() as u32).to_le_bytes());
    for (id, mat) in keys {
        debug_assert_eq!(mat.len(), key_bytes, "key material size mismatch");
        out.extend_from_slice(id.as_bytes()); // 16 B
        out.extend_from_slice(mat);
    }
    out
}

/// Serializa una lista de `key_id`s al wire (body del POST dec_keys
/// binario).
pub fn pack_key_ids(ids: &[Uuid]) -> Vec<u8> {
    let total = IDS_HDR_LEN + ids.len() * 16;
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&(ids.len() as u32).to_le_bytes());
    for id in ids {
        out.extend_from_slice(id.as_bytes());
    }
    out
}

// ───────────────────── unpack ───────────────────

/// Deserializa el wire de respuesta a una lista de `(key_id, material)`.
pub fn unpack_keys(buf: &[u8]) -> Result<Vec<(Uuid, Vec<u8>)>, BinaryError> {
    if buf.len() < KEYS_HDR_LEN {
        return Err(BinaryError::Truncated);
    }
    if buf[..4] != MAGIC {
        return Err(BinaryError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(BinaryError::BadVersion(buf[4]));
    }
    let key_size_bits = u16::from_le_bytes([buf[5], buf[6]]);
    let key_bytes = (key_size_bits as usize).div_ceil(8);
    let count = u32::from_le_bytes([buf[7], buf[8], buf[9], buf[10]]) as usize;
    let expected = KEYS_HDR_LEN + count * (16 + key_bytes);
    if buf.len() < expected {
        return Err(BinaryError::Truncated);
    }
    let mut out = Vec::with_capacity(count);
    let mut cur = KEYS_HDR_LEN;
    for _ in 0..count {
        let id = Uuid::from_slice(&buf[cur..cur + 16]).map_err(|_| BinaryError::BadUuid)?;
        cur += 16;
        let mat = buf[cur..cur + key_bytes].to_vec();
        cur += key_bytes;
        out.push((id, mat));
    }
    Ok(out)
}

/// Deserializa el body del POST dec_keys binario.
pub fn unpack_key_ids(buf: &[u8]) -> Result<Vec<Uuid>, BinaryError> {
    if buf.len() < IDS_HDR_LEN {
        return Err(BinaryError::Truncated);
    }
    if buf[..4] != MAGIC {
        return Err(BinaryError::BadMagic);
    }
    if buf[4] != VERSION {
        return Err(BinaryError::BadVersion(buf[4]));
    }
    let count = u32::from_le_bytes([buf[5], buf[6], buf[7], buf[8]]) as usize;
    let expected = IDS_HDR_LEN + count * 16;
    if buf.len() < expected {
        return Err(BinaryError::Truncated);
    }
    let mut out = Vec::with_capacity(count);
    let mut cur = IDS_HDR_LEN;
    for _ in 0..count {
        let id = Uuid::from_slice(&buf[cur..cur + 16]).map_err(|_| BinaryError::BadUuid)?;
        cur += 16;
        out.push(id);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip() {
        let k1 = (Uuid::from_u128(1), [0xaa; 32]);
        let k2 = (Uuid::from_u128(2), [0xbb; 32]);
        let buf = pack_keys(&[(k1.0, &k1.1), (k2.0, &k2.1)], 256);
        let unpacked = unpack_keys(&buf).unwrap();
        assert_eq!(unpacked.len(), 2);
        assert_eq!(unpacked[0].0, k1.0);
        assert_eq!(&unpacked[0].1[..], &k1.1[..]);
        assert_eq!(unpacked[1].0, k2.0);
        assert_eq!(&unpacked[1].1[..], &k2.1[..]);
    }

    #[test]
    fn ids_round_trip() {
        let ids = vec![
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            Uuid::from_u128(30),
        ];
        let buf = pack_key_ids(&ids);
        let back = unpack_key_ids(&buf).unwrap();
        assert_eq!(back, ids);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut buf = pack_key_ids(&[Uuid::nil()]);
        buf[0] = 0;
        assert!(matches!(unpack_key_ids(&buf), Err(BinaryError::BadMagic)));
    }

    #[test]
    fn binary_wire_is_smaller_than_json() {
        // 128 claves de 256 bits.
        let keys: Vec<(Uuid, [u8; 32])> = (0u128..128)
            .map(|i| (Uuid::from_u128(i), [0; 32]))
            .collect();
        let refs: Vec<(Uuid, &[u8])> = keys.iter().map(|(id, m)| (*id, &m[..])).collect();
        let binary = pack_keys(&refs, 256);
        // 11 + 128 * (16 + 32) = 11 + 6144 = 6155
        assert!(binary.len() < 7000);
        // El JSON equivalente sería del orden de 14 KiB.
    }
}
