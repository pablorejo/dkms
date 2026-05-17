//! Equivalente a `ETSIQKD/ETSI020/ETSI020_Message.py`.
//!
//! Estructura genérica que ETSI020 usa para respuestas de error 4xx/5xx.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020Message {
    pub message: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<serde_json::Map<String, Value>>>,
}

impl Etsi020Message {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(
        message: impl Into<String>,
        details: Vec<serde_json::Map<String, Value>>,
    ) -> Self {
        Self {
            message: message.into(),
            details: Some(details),
        }
    }
}

impl EtsiMessage for Etsi020Message {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
