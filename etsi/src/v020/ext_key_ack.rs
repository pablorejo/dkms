//! Equivalente a `ETSIQKD/ETSI020/ETSI020_ExtKeyAck.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

use super::{Etsi020AckStatus, Etsi020KeyID};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020ExtKeyAckContainer {
    pub key_ids: Vec<Etsi020KeyID>,
    pub ack_status: Etsi020AckStatus,
    pub initiator_sae_id: String,
    pub target_sae_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension: Option<serde_json::Map<String, Value>>,
}

impl EtsiMessage for Etsi020ExtKeyAckContainer {
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
