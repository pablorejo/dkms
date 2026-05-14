//! Equivalente a `ETSIQKD/ETSI020/ETSI020_postExtKeysVoid.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::{EtsiMessage, NetworkMessage};

use super::Etsi020ExtKeyVoidContainer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi020PostExtKeysVoid {
    #[serde(flatten)]
    pub body: Etsi020ExtKeyVoidContainer,

    /// Confirmación explícita para invalidar todas las claves. En
    /// pydantic es `Field(False, frozen=True, exclude=True)` (no va al
    /// wire). Aquí también lo excluimos del JSON.
    #[serde(skip)]
    pub all_confirmation: bool,
}

impl Etsi020PostExtKeysVoid {
    pub fn new(body: Etsi020ExtKeyVoidContainer, all_confirmation: bool) -> Self {
        Self { body, all_confirmation }
    }

    /// Mirror del `from_network` del Python: lee `all_confirmation`
    /// desde `url_parameters` y lo inyecta antes de validar.
    pub fn from_network(msg: &NetworkMessage) -> Option<Self> {
        let all_conf = msg
            .url_param("all_confirmation")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let data = msg.data.clone().unwrap_or(Value::Object(Default::default()));
        let body: Etsi020ExtKeyVoidContainer = serde_json::from_value(data).ok()?;

        Some(Self { body, all_confirmation: all_conf })
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

impl EtsiMessage for Etsi020PostExtKeysVoid {
    const ENDPOINT: &'static str = "/ext_keys/void";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["POST"];
    const DEFAULT_ACCESS_METHOD: &'static str = "POST";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/kmapi/v1{}", Self::ENDPOINT)
    }
}
