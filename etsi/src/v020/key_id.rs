//! Equivalente a `ETSIQKD/ETSI020/ETSI020_KeyID.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::message::EtsiMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020KeyID {
    pub key_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi020KeyID {
    pub fn new(key_id: Uuid) -> Self {
        Self { key_id, extension: None }
    }
}

impl EtsiMessage for Etsi020KeyID {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
