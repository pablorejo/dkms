//! Equivalente a `ETSIQKD/ETSI014/ETSI014_Status.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{error::EtsiError, message::EtsiMessage};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014Status {
    #[serde(rename = "source_KME_ID")]
    pub source_kme_id: String,

    #[serde(rename = "target_KME_ID")]
    pub target_kme_id: String,

    #[serde(rename = "master_SAE_ID")]
    pub master_sae_id: String,

    #[serde(rename = "slave_SAE_ID")]
    pub slave_sae_id: String,

    pub key_size: u32,
    pub stored_key_count: u64,
    pub max_key_count: u64,
    pub max_key_per_request: u32,
    pub max_key_size: u32,
    pub min_key_size: u32,

    #[serde(rename = "max_SAE_ID_count")]
    pub max_sae_id_count: u32,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi014Status {
    /// Mirror de `Field(ge=0)` para todos los enteros: los `uN` ya
    /// rechazan negativos en deserialización, así que aquí solo
    /// validamos consistencia entre min/max.
    pub fn validate(&self) -> Result<(), EtsiError> {
        if self.min_key_size > self.max_key_size {
            return Err(EtsiError::Validation(format!(
                "min_key_size ({}) > max_key_size ({})",
                self.min_key_size, self.max_key_size
            )));
        }
        if self.stored_key_count > self.max_key_count {
            return Err(EtsiError::Validation(format!(
                "stored_key_count ({}) > max_key_count ({})",
                self.stored_key_count, self.max_key_count
            )));
        }
        Ok(())
    }
}

impl EtsiMessage for Etsi014Status {
    const ENDPOINT: &'static str = "/status";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "GET";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/api/v1/keys{}", Self::ENDPOINT)
    }
}
