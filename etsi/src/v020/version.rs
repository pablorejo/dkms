//! Equivalente a `ETSIQKD/ETSI020/ETSI020_Version.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020VersionContainer {
    pub versions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<serde_json::Map<String, Value>>,
}

impl Etsi020VersionContainer {
    pub fn new(versions: Vec<String>) -> Self {
        Self {
            versions,
            extension: None,
        }
    }
}

impl EtsiMessage for Etsi020VersionContainer {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
