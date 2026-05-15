//! Codec de `header_dkms_mp`.
//!
//! Lo que el DKMS pasa al ORR como metadatos de la clave que viaja en
//! el `payload` (body_dkms = bytes crudos de la clave QKD). El ORR lo
//! propaga sin tocarlo; solo lo serializa al construir el frame y lo
//! deserializa al entregar al DKMS local en `DeliveredMessage`.
//!
//! Modelado como `BTreeMap<String, String>` por coincidir con el campo
//! `app_header` del proto. Si en el futuro queremos campos tipados
//! (key_id como UUID, key_size_bits como u32...) se convierte a struct.

use std::collections::BTreeMap;

use crate::error::{OrrError, Result};

/// Serializa un mapa `key -> value` como msgpack-named.
pub fn encode(map: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    if map.is_empty() {
        return Ok(Vec::new());
    }
    rmp_serde::to_vec_named(map)
        .map_err(|e| OrrError::Relay(format!("header_dkms msgpack encode: {e}")))
}

/// Deserializa. Acepta buffer vacío o `[0x80]` (msgpack map vacío) como
/// "sin metadatos".
pub fn decode(buf: &[u8]) -> Result<BTreeMap<String, String>> {
    if buf.is_empty() || buf == [0x80] {
        return Ok(BTreeMap::new());
    }
    rmp_serde::from_slice(buf)
        .map_err(|e| OrrError::Relay(format!("header_dkms msgpack decode: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_typical() {
        let mut m = BTreeMap::new();
        m.insert("key_id".into(), "ddd5d8a1-7a73-4dc6-9d9c-d5b8b8e9ffaa".into());
        m.insert("sae_origin".into(), "sae-1".into());
        m.insert("sae_destination".into(), "sae-2".into());
        m.insert("key_size_bits".into(), "256".into());
        m.insert("request_id".into(), "req-42".into());
        let buf = encode(&m).unwrap();
        let back = decode(&buf).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn empty_is_empty_bytes() {
        let m = BTreeMap::new();
        let buf = encode(&m).unwrap();
        assert!(buf.is_empty());
        let back = decode(&buf).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn decode_msgpack_empty_map_works() {
        let back = decode(&[0x80]).unwrap();
        assert!(back.is_empty());
    }
}
