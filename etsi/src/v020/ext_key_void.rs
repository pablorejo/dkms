//! Equivalente a `ETSIQKD/ETSI020/ESTI020_ExtKeyVoid.py`.
//! (Sí, el typo `ESTI` está en el Python original; aquí lo
//! mantenemos como `ext_key_void` siguiendo convención Rust.)

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{error::EtsiError, message::EtsiMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020ExtKeyVoidContainer {
    /// IDs de las claves a invalidar.
    pub keys: Vec<Uuid>,

    pub initiator_sae_id: String,

    /// SAEs destino. `Field(min_length=1)` se valida en
    /// [`Etsi020ExtKeyVoidContainer::validate`].
    pub target_sae_ids: Vec<String>,

    pub ack_callback_url: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi020ExtKeyVoidContainer {
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.target_sae_ids.is_empty() {
            return Err(EtsiError::Validation(
                "target_sae_ids must contain at least 1 entry".into(),
            ));
        }
        Ok(())
    }
}

impl EtsiMessage for Etsi020ExtKeyVoidContainer {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
