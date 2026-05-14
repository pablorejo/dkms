//! Equivalente a `ETSIQKD/ETSI014/ETSI014_KeyContainer.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::EtsiError, message::EtsiMessage};

use super::Etsi014Key;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014KeyContainer {
    /// Lista de claves devueltas. `Field(min_length=1)` se valida en
    /// [`Etsi014KeyContainer::validate`].
    pub keys: Vec<Etsi014Key>,

    /// Datos de extensión del contenedor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_container_extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi014KeyContainer {
    /// Endpoint por defecto cuando se construye este contenedor.
    /// Pydantic lo modela con `Literal['/enc_keys', '/dec_keys']`.
    pub const ENDPOINTS: &'static [&'static str] = &["/enc_keys", "/dec_keys"];

    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.keys.is_empty() {
            return Err(EtsiError::Validation("keys must contain at least 1 entry".into()));
        }
        for k in &self.keys {
            k.validate()?;
        }
        Ok(())
    }

    /// Equivalente a `get_keys()` del Python.
    pub fn get_keys(&self) -> &[Etsi014Key] {
        &self.keys
    }

    /// Construye la URL completa. El Python usa el endpoint guardado
    /// (`/enc_keys` o `/dec_keys`); aquí el caller pasa cuál.
    pub fn endpoint_url(&self, host: &str, endpoint: &str) -> String {
        format!("{host}/api/v1/keys{endpoint}")
    }
}

impl EtsiMessage for Etsi014KeyContainer {
    const ENDPOINT: &'static str = "/enc_keys";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["GET", "POST"];
    const DEFAULT_ACCESS_METHOD: &'static str = "GET";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/api/v1/keys{}", Self::ENDPOINT)
    }
}
