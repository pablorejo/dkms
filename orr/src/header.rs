//! Cabecera ORR v3.
//!
//! Va en `frame.header_orr_mp` (cleartext) junto al payload (lo que el QKC
//! OTP-cifrará en cada enlace). El QKC NO la toca: la propaga byte-a-byte.
//!
//! ## Diseño v3 (shared-key XOR + key_id pre-shared)
//!
//! El ORR origen y cada hop (intermedio + destino) comparten un
//! `master_secret_{from→peer}` de 32 B (establecido al arrancar via ML-KEM
//! encap, RPC `EstablishSecret`). Por cada frame onion, derivan
//! `K = HKDF-SHA256(salt="orr.onion.v1", ikm=master_secret,
//!  info=key_id ‖ u32(len(payload)), L=len(payload))` y hacen XOR.
//!
//! Esto reemplaza el esquema v2 (`OnionFrame { kem_cts: Vec<Vec<u8>>, ... }`)
//! que enviaba ~1.1 KB de ML-KEM ciphertext por cada 32 B de plaintext,
//! produciendo crecimiento exponencial por capa onion (4 MB en modo -1).
//!
//! Campos:
//!   * `from`, `to`: ids lógicos de ORR (origen final y destino final).
//!   * `next_orr_id`: dst de ESTA capa — quién pela este wire frame.
//!     Para passthrough (modo 0) = `to`.
//!   * `key_id`: UUID v4 que identifica la K usada para cifrar `payload`.
//!     `None` ⇒ passthrough (no hay capa onion, `payload` es body_dkms
//!     en claro hacia el QKC, que lo OTP-cifra en el enlace).
//!   * `max_hops`: hops onion restantes; `0` significa que la siguiente
//!     vez que un ORR descifre con K, el plaintext es body_dkms directo
//!     (capa terminal), no un `InnerLayer`.
//!   * `timestamp`: `time.time()` del emisor, segundos UNIX. Informativo.
//!
//! El metadato de la clave QKD (key_id QKD, sae_origin/destination, ...) NO
//! va aquí — va en `header_dkms_mp`, escrito por el DKMS.

use serde::{Deserialize, Serialize};

use crate::error::{OrrError, Result};

pub const HEADER_TYPE: &str = "ORR";
pub const HEADER_VERSION: u32 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OrrHeader {
    /// Discriminador. Siempre `"ORR"`. Permite distinguir frames con
    /// cabeceras de otros protocolos sobre el mismo QKC.
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    /// ORR de origen (final, no cambia hop-to-hop).
    pub from: String,
    /// ORR destino final (no cambia hop-to-hop).
    pub to: String,
    /// ORR destino de ESTA capa wire. Cambia en cada hop: el peeler usa
    /// este id para buscar el `master_secret_{from→next_orr_id}` (cuando
    /// `next_orr_id == self`, ése es siempre el caso al recibir).
    #[serde(default)]
    pub next_orr_id: String,
    /// UUID v4 de la `K` usada para cifrar el `payload`. 16 bytes raw.
    /// `None` ⇒ modo 0 passthrough (sin capa onion).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_id: Option<[u8; 16]>,
    /// Hops onion restantes después de pelar ESTA capa. `0` = capa
    /// terminal (el `payload` cifrado contiene body_dkms directo).
    pub max_hops: i32,
    /// `time.time()` del emisor, en segundos UNIX. Informativo.
    #[serde(default)]
    pub timestamp: f64,
}

impl OrrHeader {
    /// Header para modo 0 (passthrough). `key_id = None`, `max_hops = 0`,
    /// `next_orr_id == to`. El `payload` es body_dkms en claro (lo
    /// cifrará el QKC del enlace con su OTP).
    pub fn passthrough(from: &str, to: &str) -> Self {
        Self {
            kind: HEADER_TYPE.to_string(),
            version: HEADER_VERSION,
            from: from.to_string(),
            to: to.to_string(),
            next_orr_id: to.to_string(),
            key_id: None,
            max_hops: 0,
            timestamp: now_unix_secs(),
        }
    }

    /// Header para una capa onion. `payload = K ⊕ inner`, donde `K` se
    /// deriva con HKDF a partir de `master_secret_{from→next_orr_id}` y
    /// el `key_id` que va aquí.
    pub fn onion(
        from: &str,
        to: &str,
        next_orr_id: &str,
        key_id: [u8; 16],
        max_hops: i32,
    ) -> Self {
        Self {
            kind: HEADER_TYPE.to_string(),
            version: HEADER_VERSION,
            from: from.to_string(),
            to: to.to_string(),
            next_orr_id: next_orr_id.to_string(),
            key_id: Some(key_id),
            max_hops,
            timestamp: now_unix_secs(),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self)
            .map_err(|e| OrrError::Relay(format!("header msgpack encode: {e}")))
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        if buf.is_empty() || buf == [0x80] {
            return Ok(Self::default());
        }
        rmp_serde::from_slice(buf)
            .map_err(|e| OrrError::Relay(format!("header msgpack decode: {e}")))
    }
}

fn now_unix_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_round_trip() {
        let h = OrrHeader::passthrough("orr_1", "orr_3");
        let buf = h.encode().unwrap();
        let h2 = OrrHeader::decode(&buf).unwrap();
        assert_eq!(h2.kind, HEADER_TYPE);
        assert_eq!(h2.version, 3);
        assert_eq!(h2.from, "orr_1");
        assert_eq!(h2.to, "orr_3");
        assert_eq!(h2.next_orr_id, "orr_3");
        assert!(h2.key_id.is_none());
        assert_eq!(h2.max_hops, 0);
    }

    #[test]
    fn onion_round_trip() {
        let kid = [0xAB; 16];
        let h = OrrHeader::onion("orr_1", "orr_4", "orr_2", kid, 2);
        let buf = h.encode().unwrap();
        let h2 = OrrHeader::decode(&buf).unwrap();
        assert_eq!(h2.from, "orr_1");
        assert_eq!(h2.to, "orr_4");
        assert_eq!(h2.next_orr_id, "orr_2");
        assert_eq!(h2.key_id, Some(kid));
        assert_eq!(h2.max_hops, 2);
    }

    #[test]
    fn empty_buf_decodes_to_default() {
        let h = OrrHeader::decode(&[]).unwrap();
        assert_eq!(h.from, "");
        assert_eq!(h.max_hops, 0);
        let h2 = OrrHeader::decode(&[0x80]).unwrap();
        assert_eq!(h2.from, "");
    }
}
