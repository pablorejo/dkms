//! Cabecera ORR.
//!
//! Cuando el ORR envía un payload al QKC vía `FRAME_LOCAL_SEND`, mete
//! una cabecera msgpack en el campo `header_mp` del frame. El QKC la
//! propaga sin tocarla (es payload para él). Al llegar al ORR destino,
//! éste la deserializa para saber:
//!
//! * `from`, `to`: ids lógicos de ORR (no de QKC) — el QKC ya hizo el
//!   routing a nivel de su `dest_final`, pero el ORR puede querer ver
//!   "vengo de tal ORR".
//! * `max_hops`: hops cebolla que faltan (0 = ya estoy en el destino o
//!   modo passthrough).
//! * `app_header`: metadatos que el cliente DKMS quiere transportar.
//! * `pqc_layer`: si está a `true`, hay una capa PQC end-to-end por
//!   descifrar antes de entregar a la capa de aplicación
//!   (`max_hops == 1`).
//! * `sdn_path`: cola de QKC ids que faltan por visitar (cebolla guiada
//!   por la SDN).
//!
//! Mantenemos los nombres de campos compatibles con la implementación
//! Python para facilitar mixed-deployment (un nodo Rust hablando con
//! uno Python via QKC).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{OrrError, Result};

pub const HEADER_TYPE: &str = "ORR";
pub const HEADER_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OrrHeader {
    /// Discriminador. Siempre `"ORR"`. Permite que el QKC enrute frames
    /// con cabeceras de otros protocolos sin que el ORR los procese.
    #[serde(rename = "type")]
    pub kind: String,
    pub version: u32,
    /// ORR de origen.
    pub from: String,
    /// ORR destino final.
    pub to: String,
    /// Hops cebolla restantes.
    pub max_hops: i32,
    /// `time.time()` del emisor, en segundos UNIX (compatible con
    /// Python). Informativo.
    #[serde(default)]
    pub timestamp: f64,
    /// Si `true`, este paquete lleva una capa PQC end-to-end con el
    /// ORR destino. El ORR destino tiene que decapsular antes de
    /// entregar.
    #[serde(default, skip_serializing_if = "is_false")]
    pub pqc_layer: bool,
    /// Cuando hay capa PQC, qué ORR es el receptor (informativo, suele
    /// coincidir con `to`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pqc_destination_orr: Option<String>,
    /// Cola de QKC ids restantes por visitar en la cebolla SDN-driven
    /// (sólo `max_hops != 0`). Se almacenan como strings para mantener
    /// la compatibilidad con la implementación Python.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sdn_path: Vec<String>,
    /// `flow_id` promovido al top-level cuando viene en `app_header`,
    /// para que el QKC (que indexa su tabla por `flow_id`) lo encuentre
    /// sin tener que abrir el `app_header` anidado.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_id: Option<String>,
    /// Metadatos que el cliente (DKMS) pasa a la aplicación destino.
    /// Lo dejamos como `BTreeMap<String, String>` para coincidir con
    /// `map<string, string> app_header` del proto y para tener orden
    /// determinista al serializar.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub app_header: BTreeMap<String, String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl OrrHeader {
    pub fn new(from: &str, to: &str, max_hops: i32) -> Self {
        Self {
            kind: HEADER_TYPE.to_string(),
            version: HEADER_VERSION,
            from: from.to_string(),
            to: to.to_string(),
            max_hops,
            timestamp: now_unix_secs(),
            ..Default::default()
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self)
            .map_err(|e| OrrError::Relay(format!("header msgpack encode: {e}")))
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        // Cabecera vacía válida (`0x80` = map vacío msgpack): produce
        // un header default — útil para los tests del QKC que envían
        // `header_mp = vec![0x80]`.
        if buf == [0x80] {
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
    fn round_trip_minimal() {
        let h = OrrHeader::new("ORR_1", "ORR_3", 0);
        let buf = h.encode().unwrap();
        let h2 = OrrHeader::decode(&buf).unwrap();
        assert_eq!(h2.kind, HEADER_TYPE);
        assert_eq!(h2.from, "ORR_1");
        assert_eq!(h2.to, "ORR_3");
        assert_eq!(h2.max_hops, 0);
    }

    #[test]
    fn round_trip_with_app_header() {
        let mut h = OrrHeader::new("ORR_1", "ORR_2", 0);
        h.app_header.insert("flow_id".into(), "abc".into());
        h.flow_id = Some("abc".into());
        let buf = h.encode().unwrap();
        let h2 = OrrHeader::decode(&buf).unwrap();
        assert_eq!(h2.app_header.get("flow_id").map(String::as_str), Some("abc"));
        assert_eq!(h2.flow_id.as_deref(), Some("abc"));
    }

    #[test]
    fn empty_map_decodes_to_default() {
        let h = OrrHeader::decode(&[0x80]).unwrap();
        assert_eq!(h.from, "");
        assert_eq!(h.max_hops, 0);
    }
}
