//! Equivalente a `ETSIQKD/ETSI014/ETSI014_getStatus.py`.

use serde::{Deserialize, Serialize};

use crate::message::{EtsiMessage, NetworkMessage};

/// Request a `/api/v1/keys/{SAE_id}/status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014GetStatus {
    #[serde(rename = "SAE_id")]
    pub sae_id: String,
}

impl Etsi014GetStatus {
    pub fn new(sae_id: impl Into<String>) -> Self {
        Self {
            sae_id: sae_id.into(),
        }
    }

    /// Equivalente al `from_network` del Python: extrae el `SAE_id` del
    /// penúltimo segmento del path (`/api/v1/keys/{SAE_id}/status`).
    pub fn from_network(msg: &NetworkMessage) -> Option<Self> {
        let sae = msg.path_second_to_last();
        if sae.is_empty() {
            None
        } else {
            Some(Self::new(sae))
        }
    }
}

impl EtsiMessage for Etsi014GetStatus {
    const ENDPOINT: &'static str = "/status";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "GET";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/api/v1/keys/{}{}", self.sae_id, Self::ENDPOINT)
    }

    /// Python sobrescribe `to_json` para devolver `''`. Mantenemos.
    fn to_json(&self) -> crate::error::Result<String> {
        Ok(String::new())
    }
}
