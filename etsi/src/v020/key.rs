//! Equivalente a `ETSIQKD/ETSI020/ETSI020_Key.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{base64bytes::Base64Bytes, error::EtsiError, message::EtsiMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020Key {
    pub key_id: Uuid,
    /// Valor de la clave (base64 en wire, bytes crudos en memoria).
    /// `Field(min_length=1)` se valida en [`Etsi020Key::validate`].
    pub value: Base64Bytes,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi020Key {
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.value.is_empty() {
            return Err(EtsiError::Validation("value must be at least 1 byte".into()));
        }
        Ok(())
    }

    /// Mirror de `get_key()` en Python.
    pub fn get_key(&self) -> String {
        self.value.to_b64_string()
    }
}

impl EtsiMessage for Etsi020Key {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
