//! Equivalente a `ETSIQKD/ETSI014/ETSI014_KeyID.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::message::EtsiMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014KeyID {
    #[serde(rename = "key_ID")]
    pub key_id: Uuid,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "key_ID_extension"
    )]
    pub key_id_extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi014KeyID {
    pub fn new(key_id: Uuid) -> Self {
        Self {
            key_id,
            key_id_extension: None,
        }
    }
}

impl EtsiMessage for Etsi014KeyID {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
