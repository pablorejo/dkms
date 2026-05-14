//! Equivalente a `ETSIQKD/ETSI020/ETSI020_getVersions.py`.

use serde::{Deserialize, Serialize};

use crate::{
    error::Result,
    message::{EtsiMessage, NetworkMessage},
};

/// Request a `/kmapi/versions`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Etsi020GetVersions;

impl Etsi020GetVersions {
    pub fn from_network(_msg: &NetworkMessage) -> Option<Self> {
        Some(Self)
    }
}

impl EtsiMessage for Etsi020GetVersions {
    const ENDPOINT: &'static str = "/versions";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &["GET"];
    const DEFAULT_ACCESS_METHOD: &'static str = "GET";

    fn get_endpoint_url(&self, host: &str) -> String {
        format!("{host}/kmapi{}", Self::ENDPOINT)
    }

    /// Python sobrescribe a `''`. Mantenemos.
    fn to_json(&self) -> Result<String> {
        Ok(String::new())
    }
}
