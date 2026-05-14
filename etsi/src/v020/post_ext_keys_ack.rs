//! Equivalente a `ETSIQKD/ETSI020/ETSI020_postExtKeysAck.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

use super::Etsi020ExtKeyAckContainer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020PostExtKeysAck {
    #[serde(flatten)]
    pub body: Etsi020ExtKeyAckContainer,
}

impl Etsi020PostExtKeysAck {
    pub fn new(body: Etsi020ExtKeyAckContainer) -> Self {
        Self { body }
    }

    /// Mirror del `add_extension` del Python.
    pub fn add_extension(
        &mut self,
        name: impl Into<String>,
        data: serde_json::Map<String, Value>,
    ) {
        let target = self.body.extension.get_or_insert_with(serde_json::Map::new);
        target.insert(name.into(), Value::Object(data));
    }
}

impl EtsiMessage for Etsi020PostExtKeysAck {
    const ENDPOINT: &'static str = "/ext_keys/ack";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["POST"];
    const DEFAULT_ACCESS_METHOD: &'static str = "POST";

    /// Mirror del Python: si el host ya termina en `/kmapi/v1/ext_keys/ack`
    /// (porque viene de un callback URL) no lo añadimos otra vez.
    fn get_endpoint_url(&self, host: &str) -> String {
        let suffix = format!("/kmapi/v1{}", Self::ENDPOINT);
        if host.ends_with(&suffix) {
            host.to_string()
        } else {
            format!("{host}{suffix}")
        }
    }
}
