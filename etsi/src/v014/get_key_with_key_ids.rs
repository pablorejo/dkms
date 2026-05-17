//! Equivalente a `ETSIQKD/ETSI014/ETSI014_getKeyWithKeyIDs.py`.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    error::Result,
    message::{EtsiMessage, NetworkMessage},
};

use super::{Etsi014KeyID, Etsi014KeyIDs};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014GetKeyWithKeyIDs {
    #[serde(rename = "SAE_id")]
    pub sae_id: String,

    #[serde(rename = "key_IDs", flatten)]
    pub key_ids: Etsi014KeyIDs,

    #[serde(default = "default_method", skip_serializing)]
    pub access_method: String,
}

fn default_method() -> String {
    "POST".into()
}

impl Etsi014GetKeyWithKeyIDs {
    pub fn new(sae_id: impl Into<String>, key_ids: Etsi014KeyIDs) -> Self {
        Self {
            sae_id: sae_id.into(),
            key_ids,
            access_method: default_method(),
        }
    }

    /// Acceso al listado de IDs (mirror de `get_ids` en Python).
    pub fn get_ids(&self) -> &[Etsi014KeyID] {
        &self.key_ids.key_ids
    }

    /// Equivalente al `from_network` del Python.
    ///
    /// * **GET** — espera `url_parameters['key_ID']`. Devuelve `None`
    ///   si falta.
    /// * **POST** — body JSON de `ETSI014_KeyIDs`.
    pub fn from_network(msg: &NetworkMessage) -> Option<Self> {
        let method = msg.method.as_deref().unwrap_or("POST").to_uppercase();
        let sae = msg.path_second_to_last().to_string();
        if sae.is_empty() {
            return None;
        }

        if method == "GET" {
            let key_id_value = msg.url_param("key_ID")?;
            let key_id_str = key_id_value.as_str()?;
            let parsed = Uuid::parse_str(key_id_str).ok()?;
            return Some(Self {
                sae_id: sae,
                key_ids: Etsi014KeyIDs::new(vec![Etsi014KeyID::new(parsed)]),
                access_method: "POST".into(),
            });
        }

        let data = msg
            .data
            .clone()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let key_ids: Etsi014KeyIDs = serde_json::from_value(data).ok()?;

        Some(Self {
            sae_id: sae,
            key_ids,
            access_method: method,
        })
    }
}

impl EtsiMessage for Etsi014GetKeyWithKeyIDs {
    const ENDPOINT: &'static str = "/dec_keys";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["POST", "GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "POST";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/api/v1/keys/{}{}", self.sae_id, Self::ENDPOINT)
    }

    /// Mirror del Python: `to_json` devuelve sólo el cuerpo `key_IDs`.
    fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string(&self.key_ids)?)
    }
}
