//! Equivalente a `ETSIQKD/ETSI014/ETSI014_Error.py`.
//!
//! Formato de error para `/status`, `/enc_keys`, `/dec_keys`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::EtsiMessage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Etsi014Error {
    /// Mensaje de error.
    pub message: String,

    /// Detalles opcionales. Acepta tanto lista de objetos como lista de
    /// strings — exactamente como el `Union[List[dict], List[str]]` del
    /// Python.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Etsi014ErrorDetails>,
}

/// Mirror del `Union[List[Dict[str, Any]], List[str]]` del Python.
/// Pydantic acepta cualquiera de los dos; serde lo hace via `untagged`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Etsi014ErrorDetails {
    Objects(Vec<serde_json::Map<String, Value>>),
    Strings(Vec<String>),
}

impl EtsiMessage for Etsi014Error {
    // El Error no tiene endpoint propio: se sirve como response body
    // de cualquiera de los tres endpoints. Mantenemos string vacío
    // como hace el Python al heredar de ETSI_message sin sobrescribir.
    const ENDPOINT: &'static str = "";
    const AVAILABLE_ACCESS_METHODS: &'static [&'static str] = &[];
    const DEFAULT_ACCESS_METHOD: &'static str = "";
}
