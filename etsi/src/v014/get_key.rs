//! Equivalente a `ETSIQKD/ETSI014/ETSI014_getKey.py`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    error::Result,
    message::{EtsiMessage, NetworkMessage},
};

use super::Etsi014KeyRequest;

/// Request a `/api/v1/keys/{SAE_id}/enc_keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014GetKey {
    #[serde(rename = "SAE_id")]
    pub sae_id: String,

    /// Cuerpo `ETSI014_KeyRequest`. Para GET puede llegar vacío y
    /// rellenarse con defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Etsi014KeyRequest>,

    /// Método HTTP con el que llegó la request. En Python es un campo
    /// `Field(...)` validado; aquí va expuesto para que el caller lo
    /// inspeccione si lo necesita.
    #[serde(default = "default_method", skip_serializing)]
    pub access_method: String,
}

fn default_method() -> String {
    "POST".into()
}

impl Etsi014GetKey {
    pub fn new(sae_id: impl Into<String>, request: Option<Etsi014KeyRequest>) -> Self {
        Self {
            sae_id: sae_id.into(),
            request,
            access_method: default_method(),
        }
    }

    /// Equivalente al `from_network` del Python.
    ///
    /// Maneja dos modos:
    ///
    /// * **GET** — los parámetros vienen en `url_parameters` (`number`,
    ///   `size`, `additional_saes`). Se mergea con la cabecera
    ///   `additional_saes` si está.
    /// * **POST** — `data` es el cuerpo JSON de `ETSI014_KeyRequest`;
    ///   también se mergea con la cabecera.
    pub fn from_network(msg: &NetworkMessage) -> Option<Self> {
        let method = msg.method.as_deref().unwrap_or("POST").to_uppercase();
        let sae = msg.path_second_to_last().to_string();
        if sae.is_empty() {
            return None;
        }

        let header_additional = msg
            .header_ci("additional_saes")
            .or_else(|| msg.header_ci("additional-saes"))
            .or_else(|| msg.header_ci("x-additional-saes"))
            .cloned();

        let req = if method == "GET" {
            let number = msg
                .url_param("number")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u32;
            let size = msg
                .url_param("size")
                .and_then(|v| v.as_u64())
                .unwrap_or(256) as u32;

            let query_additional = msg
                .url_param("additional_saes")
                .or_else(|| msg.url_param("additional-saes"))
                .or_else(|| msg.url_param("additional_slave_SAE_IDs"))
                .cloned();

            let additional_saes = query_additional
                .as_ref()
                .map(Etsi014KeyRequest::normalize_sae_list)
                .filter(|v| !v.is_empty());

            let mut r = Etsi014KeyRequest {
                number,
                size,
                additional_slave_sae_ids: None,
                additional_saes,
                extension_mandatory: None,
                extension_optional: None,
            };

            if let Some(h) = &header_additional {
                let merged = r.resolved_additional_slave_sae_ids(Some(h));
                r.additional_saes = if merged.is_empty() {
                    None
                } else {
                    Some(merged)
                };
            }
            r
        } else {
            let data = msg
                .data
                .clone()
                .unwrap_or(Value::Object(Default::default()));
            let mut r: Etsi014KeyRequest = serde_json::from_value(data).unwrap_or_default();
            if let Some(h) = &header_additional {
                let merged = r.resolved_additional_slave_sae_ids(Some(h));
                r.additional_saes = if merged.is_empty() {
                    None
                } else {
                    Some(merged)
                };
            }
            r
        };

        Some(Self {
            sae_id: sae,
            request: Some(req),
            access_method: method,
        })
    }
}

impl EtsiMessage for Etsi014GetKey {
    const ENDPOINT: &'static str = "/enc_keys";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["POST", "GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "POST";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/api/v1/keys/{}{}", self.sae_id, Self::ENDPOINT)
    }

    /// Mirror del `to_json` sobrescrito del Python: serializa solo el
    /// cuerpo `request`, no el envoltorio.
    fn to_json(&self) -> Result<String> {
        match &self.request {
            Some(r) => Ok(serde_json::to_string(r)?),
            None => Ok("{}".into()),
        }
    }
}
