//! Equivalente a `ETSIQKD/ETSI014/ETSI014_KeyIDs.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

use super::Etsi014KeyID;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014KeyIDs {
    /// Lista de IDs.
    #[serde(rename = "key_IDs")]
    pub key_ids: Vec<Etsi014KeyID>,

    /// Extensión de la lista de IDs.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "key_IDs_extension"
    )]
    pub key_ids_extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi014KeyIDs {
    pub fn new(ids: Vec<Etsi014KeyID>) -> Self {
        Self {
            key_ids: ids,
            key_ids_extension: None,
        }
    }
}

impl EtsiMessage for Etsi014KeyIDs {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
