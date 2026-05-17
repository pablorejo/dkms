//! Equivalente a `ETSIQKD/ETSI014/ETSI014.py` — la factoría
//! `from_network` que enruta un `NetworkMessage` al subtipo correcto.

use crate::message::NetworkMessage;

use super::{
    Etsi014Error, Etsi014GetKey, Etsi014GetKeyWithKeyIDs, Etsi014GetStatus, Etsi014KeyContainer,
    Etsi014Status,
};

/// Códigos HTTP que en Python disparan la rama "es error".
/// (`ETSI014.AVAILABLE_ERROR_CODES`).
pub const AVAILABLE_ERROR_CODES: &[i32] = &[400, 501, 503];

/// Enum que cubre todos los subtipos que la factoría puede producir.
/// Replica el comportamiento del `from_network` Python que devolvía
/// `Optional[ETSI_message]` con la subclase concreta.
#[derive(Debug, Clone)]
pub enum Etsi014Built {
    /// Request: `/status`.
    GetStatus(Etsi014GetStatus),
    /// Request: `/enc_keys`.
    GetKey(Etsi014GetKey),
    /// Request: `/dec_keys`.
    GetKeyWithKeyIDs(Etsi014GetKeyWithKeyIDs),

    /// Response: `/status`.
    Status(Etsi014Status),
    /// Response: `/enc_keys` o `/dec_keys`.
    KeyContainer(Etsi014KeyContainer),
    /// Response: error 4xx/5xx en cualquiera de los tres endpoints.
    Error(Etsi014Error),
}

/// Factoría — equivalente a la clase `ETSI014` del Python con su único
/// método estático `from_network`.
pub struct Etsi014;

impl Etsi014 {
    /// Construye el subtipo apropiado a partir del network message.
    ///
    /// El enrutado replica la lógica del Python:
    ///
    /// * Si `is_response == True`:
    ///   - status_code ∈ `AVAILABLE_ERROR_CODES`  → `Etsi014Error`.
    ///   - endpoint `"status"`                     → `Etsi014Status`.
    ///   - endpoint `"enc_keys"` / `"dec_keys"`    → `Etsi014KeyContainer`.
    /// * Si es request:
    ///   - endpoint `"status"`    → `Etsi014GetStatus`.
    ///   - endpoint `"enc_keys"`  → `Etsi014GetKey`.
    ///   - endpoint `"dec_keys"`  → `Etsi014GetKeyWithKeyIDs`.
    ///
    /// Devuelve `None` si no se reconoce ninguno (mismo comportamiento
    /// que el Python ante un `ValidationError` no recuperable).
    pub fn from_network(msg: &NetworkMessage) -> Option<Etsi014Built> {
        let endpoint = msg.endpoint.as_deref().unwrap_or("");
        let status_code = msg.status_code;

        if msg.is_response {
            if matches!(endpoint, "status" | "enc_keys" | "dec_keys") {
                if let Some(code) = status_code {
                    if AVAILABLE_ERROR_CODES.contains(&code) {
                        if let Some(data) = &msg.data {
                            if let Ok(err) = serde_json::from_value::<Etsi014Error>(data.clone()) {
                                return Some(Etsi014Built::Error(err));
                            }
                        }
                        return None;
                    }
                }
            }
            match endpoint {
                "status" => msg
                    .data
                    .as_ref()
                    .and_then(|d| serde_json::from_value::<Etsi014Status>(d.clone()).ok())
                    .map(Etsi014Built::Status),
                "enc_keys" | "dec_keys" => msg
                    .data
                    .as_ref()
                    .and_then(|d| serde_json::from_value::<Etsi014KeyContainer>(d.clone()).ok())
                    .map(Etsi014Built::KeyContainer),
                _ => None,
            }
        } else {
            match endpoint {
                "status" => Etsi014GetStatus::from_network(msg).map(Etsi014Built::GetStatus),
                "enc_keys" => Etsi014GetKey::from_network(msg).map(Etsi014Built::GetKey),
                "dec_keys" => {
                    Etsi014GetKeyWithKeyIDs::from_network(msg).map(Etsi014Built::GetKeyWithKeyIDs)
                }
                _ => None,
            }
        }
    }
}
