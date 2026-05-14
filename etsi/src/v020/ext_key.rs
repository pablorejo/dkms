//! Equivalente a `ETSIQKD/ETSI020/ETSI020_ExtKey.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::EtsiError, message::EtsiMessage};

use super::Etsi020Key;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020ExtKeyContainer {
    /// Claves a entregar. `Field(min_length=1)` se valida en
    /// [`Etsi020ExtKeyContainer::validate`].
    pub keys: Vec<Etsi020Key>,

    pub initiator_sae_id: String,

    /// SAEs destino. `Field(min_length=1)` se valida en
    /// [`Etsi020ExtKeyContainer::validate`].
    pub target_sae_ids: Vec<String>,

    pub ack_callback_url: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_mandatory: Option<serde_json::Map<String, Value>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_optional: Option<serde_json::Map<String, Value>>,
}

impl Etsi020ExtKeyContainer {
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.keys.is_empty() {
            return Err(EtsiError::Validation("keys must contain at least 1 entry".into()));
        }
        if self.target_sae_ids.is_empty() {
            return Err(EtsiError::Validation(
                "target_sae_ids must contain at least 1 entry".into(),
            ));
        }
        for k in &self.keys {
            k.validate()?;
        }
        Ok(())
    }
}

impl EtsiMessage for Etsi020ExtKeyContainer {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
