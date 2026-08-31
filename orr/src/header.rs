//! Cabecera ORR v3.
//!
//! Va en `frame.header_orr_mp` (cleartext) junto al payload (lo que el QKC
//! OTP-cifrará en cada enlace). El QKC NO la toca: la propaga byte-a-byte —
//! incluida `frame.epoch_id` a nivel de wire, que dice qué época del
//! `master_secret` pela esta capa (el relay la reconstruía sin copiarla y
//! toda cebolla llegaba como época 0; arreglado 2026-08-30 en
//! `qkc/src/relay.rs`).
//!
//! ## Diseño v3 (AEAD por capa sobre secreto compartido; antes XOR)
//!
//! El ORR origen y cada hop (intermedio + destino) comparten un
//! `master_secret_{from→peer}` de 32 B por ÉPOCA (bootstrap ML-KEM
//! `EstablishSecret` + rotación periódica, `rotation.rs`). Por cada capa el
//! origen elige un `key_id` UUID v4 y la sella con AES-256-GCM: clave y nonce
//! derivados de `(master_secret, key_id)` —el nonce NO viaja, así que cada K
//! se usa con exactamente un nonce— y AAD que ata la capa a lo que la
//! acompaña en claro (`key_id`, `epoch_id`, `max_hops`, `session`, `counter`
//! y la cabecera DKMS). Detalles y racional en `onion.rs`.
//!
//! Hasta 2026-08-28 la capa era XOR puro con la K (maleable: un QKC del
//! camino podía aplicar un delta sin que nada lo notase); el esquema v2
//! anterior (~1,1 KB de ML-KEM ciphertext por capa) murió antes.
//!
//! El **tag AEAD viaja en esta cabecera** (campo [`OrrHeader::tag`]), no
//! pegado al payload: el tag no es secreto y en el payload cada byte cuesta
//! material QKD por salto — el chunker OTP gasta una clave por bloque de
//! `key_size_bits/8`, y meter `nonce ‖ tag` dentro midió un −51 % de
//! rendimiento en el brazo QKD.
//!
//! Campos: ver los docs de [`OrrHeader`] — `from`/`to`/`next_orr_id`
//! (routing), `key_id` (deriva K y nonce; `None` ⇒ modo 0 passthrough),
//! `max_hops`, `session`/`counter` (frescura extremo a extremo del ORIGEN,
//! dentro del AAD de todas las capas; ver `onion_replay`), `tag` y
//! `timestamp` (informativo).
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
    /// Encarnación del ORR de ORIGEN (`from`), aleatoria por arranque de
    /// proceso. No cambia salto a salto: quien reenvía la copia tal cual.
    #[serde(default)]
    pub session: u64,
    /// Contador monotónico del origen, uno por mensaje. Junto con `session` es
    /// la frescura extremo a extremo: los dos entran en el AAD de todas las
    /// capas, así que un reenviador no puede cambiarlos sin romper el tag del
    /// salto siguiente. Ver `onion_replay`.
    #[serde(default)]
    pub counter: u64,
    /// Tag AES-GCM de la capa que va en `payload`.
    ///
    /// Viaja en la cabecera y no pegado al payload a propósito: el tag no es
    /// secreto, y el OTP del enlace QKC trocea el payload en bloques de
    /// `key_size_bits / 8` gastando una clave QKD por bloque. Metido en el
    /// payload, 16 bytes de tag convierten un mensaje de 32 en dos bloques y
    /// duplican el consumo de material por salto (medido: −51 % de rendimiento).
    #[serde(with = "serde_bytes", default)]
    pub tag: [u8; common::crypto::aead::TAG_LEN],
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
            // Passthrough no lleva capa onion, así que no hay AAD que atar ni
            // ventana que consultar; los campos van a 0 por uniformidad.
            session: 0,
            counter: 0,
            tag: [0u8; common::crypto::aead::TAG_LEN],
        }
    }

    /// Header para una capa onion. `payload = K ⊕ inner`, donde `K` se
    /// deriva con HKDF a partir de `master_secret_{from→next_orr_id}` y
    /// el `key_id` que va aquí.
    #[allow(clippy::too_many_arguments)]
    pub fn onion(
        from: &str,
        to: &str,
        next_orr_id: &str,
        key_id: [u8; 16],
        max_hops: i32,
        session: u64,
        counter: u64,
        tag: [u8; common::crypto::aead::TAG_LEN],
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
            session,
            counter,
            tag,
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
        let h = OrrHeader::onion("orr_1", "orr_4", "orr_2", kid, 2, 9, 3, [7u8; 16]);
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
