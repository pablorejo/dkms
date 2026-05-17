//! Equivalente al `ETSIQKD/ETSI_message.py` del Python.
//!
//! En pydantic, `ETSI_message` es la clase base con tres campos
//! `exclude=True`: `endpoint`, `available_access_methods`,
//! `access_method`, más métodos `to_json`, `from_json`, `get_endpoint_url`.
//!
//! En Rust se separa en dos piezas:
//!
//! * El trait [`EtsiMessage`] con la metadata de cada tipo (constantes
//!   asociadas) y métodos `to_json` / `get_endpoint_url`.
//! * El struct [`NetworkMessage`] que captura lo que las `from_network`
//!   reciben (método HTTP, path, headers, body, status_code, etc.) — en
//!   Python es duck-typed, aquí es explícito.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// Trait implementado por todos los mensajes ETSI 014 / 020.
///
/// Las constantes asociadas reemplazan los campos `exclude=True` del
/// Python (`endpoint`, `available_access_methods`, `access_method`),
/// que nunca cruzan al wire.
pub trait EtsiMessage: Serialize {
    /// Endpoint relativo, p.ej. `/status`, `/enc_keys`, `/versions`.
    const ENDPOINT: &'static str;

    /// Métodos HTTP soportados para este mensaje.
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str];

    /// Método HTTP por defecto cuando se construye el mensaje.
    const DEFAULT_ACCESS_METHOD: &'static str;

    /// Serializa a JSON omitiendo campos `Option::None`
    /// (equivalente a `model_dump_json(exclude_none=True)` de pydantic).
    fn to_json(&self) -> Result<String> {
        serde_json::to_string(self).map_err(Into::into)
    }

    /// Construye la URL completa del endpoint para un host dado.
    /// Cada tipo lo sobrecarga porque la ruta concreta varía (p.ej.
    /// `/api/v1/keys/{SAE_id}/status` vs `/kmapi/versions`).
    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}{}", Self::ENDPOINT)
    }
}

/// Representación duck-typed del "network message" que pydantic
/// recibía como `dict | object` en `from_network`.
///
/// Todos los campos son opcionales porque las distintas factorías
/// inspeccionan unos u otros. Los nombres son los del Python.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkMessage {
    #[serde(default, rename = "isResponse")]
    pub is_response: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    #[serde(default)]
    pub path: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i32>,

    #[serde(default)]
    pub url_parameters: HashMap<String, Value>,

    #[serde(default)]
    pub headers: HashMap<String, Value>,

    /// Body del request o response. Si es JSON, `Value::Object(...)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl NetworkMessage {
    pub fn new() -> Self {
        Self::default()
    }

    /// Atajo equivalente a `url_parameters.get(key)` del Python.
    pub fn url_param(&self, key: &str) -> Option<&Value> {
        self.url_parameters.get(key)
    }

    /// Atajo case-insensitive sobre headers (`headers_lc` del Python).
    pub fn header_ci(&self, key: &str) -> Option<&Value> {
        let lower = key.to_ascii_lowercase();
        self.headers
            .iter()
            .find_map(|(k, v)| (k.to_ascii_lowercase() == lower).then_some(v))
    }

    /// Devuelve `data` como objeto JSON (`Map<String, Value>`) o un map
    /// vacío si no está presente o no es objeto.
    pub fn data_as_object(&self) -> serde_json::Map<String, Value> {
        match &self.data {
            Some(Value::Object(m)) => m.clone(),
            _ => serde_json::Map::new(),
        }
    }

    /// Helper: penúltimo segmento del `path` (Python hace
    /// `path.split('/')[-2]` para extraer `{SAE_id}` del path
    /// `/api/v1/keys/{SAE_id}/status`).
    pub fn path_second_to_last(&self) -> &str {
        let segs: Vec<&str> = self.path.split('/').collect();
        if segs.len() >= 2 {
            segs[segs.len() - 2]
        } else {
            ""
        }
    }
}
