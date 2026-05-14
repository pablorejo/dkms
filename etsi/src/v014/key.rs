//! Equivalente a `ETSIQKD/ETSI014/ETSI014_Key.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{base64bytes::Base64Bytes, error::EtsiError, message::EtsiMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014Key {
    /// Identificador de la clave.
    #[serde(rename = "key_ID")]
    pub key_id: Uuid,

    /// Datos de extensión asociados al ID.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "key_ID_extension")]
    pub key_id_extension: Option<serde_json::Map<String, Value>>,

    /// Valor de la clave (base64 en wire, bytes crudos en memoria).
    /// `Field(min_length=1)` se valida en [`Etsi014Key::validate`].
    pub key: Base64Bytes,

    /// Datos de extensión asociados a la clave.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi014Key {
    /// Misma semántica que `pydantic.Field(min_length=1)` sobre el campo
    /// `key`: rechaza claves vacías.
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.key.is_empty() {
            return Err(EtsiError::Validation("key must be at least 1 byte".into()));
        }
        Ok(())
    }

    /// Devuelve la clave como string base64 — mirror de `get_key()` en
    /// Python (que devuelve `model_dump()['key'].decode()`).
    pub fn get_key(&self) -> String {
        self.key.to_b64_string()
    }
}

impl EtsiMessage for Etsi014Key {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
