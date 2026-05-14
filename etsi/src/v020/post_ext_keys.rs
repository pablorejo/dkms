//! Equivalente a `ETSIQKD/ETSI020/ETSI020_postExtKeys.py`.
//!
//! En Python hereda de `Etsi020_ExtKeyContainer` y sobrescribe el
//! `endpoint`. En Rust mantenemos composición (`body`) en lugar de
//! herencia.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

use super::Etsi020ExtKeyContainer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020PostExtKeys {
    #[serde(flatten)]
    pub body: Etsi020ExtKeyContainer,
}

impl Etsi020PostExtKeys {
    pub fn new(body: Etsi020ExtKeyContainer) -> Self {
        Self { body }
    }

    /// Mirror del `add_extension` del Python: añade al map de
    /// extensiones mandatorias u opcionales.
    pub fn add_extension(
        &mut self,
        name: impl Into<String>,
        data: serde_json::Map<String, Value>,
        is_mandatory: bool,
    ) {
        let target = if is_mandatory {
            self.body
                .extension_mandatory
                .get_or_insert_with(serde_json::Map::new)
        } else {
            self.body
                .extension_optional
                .get_or_insert_with(serde_json::Map::new)
        };
        target.insert(name.into(), Value::Object(data));
    }
}

impl EtsiMessage for Etsi020PostExtKeys {
    const ENDPOINT: &'static str = "/ext_keys";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["POST"];
    const DEFAULT_ACCESS_METHOD: &'static str = "POST";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/kmapi/v1{}", Self::ENDPOINT)
    }
}
