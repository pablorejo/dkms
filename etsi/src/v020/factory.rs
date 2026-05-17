//! Equivalente a `ETSIQKD/ETSI020/ETSI020.py` — la factoría
//! `from_network` que enruta un `NetworkMessage` al subtipo correcto.

use serde_json::Value;

use crate::message::NetworkMessage;

use super::{Etsi020GetVersions, Etsi020Message, Etsi020PostExtKeysVoid, Etsi020VersionContainer};

/// Códigos HTTP que en Python disparan la rama "es error".
/// (`ETSI020.AVAILABLE_ERROR_CODES`).
pub const AVAILABLE_ERROR_CODES: &[i32] = &[400, 401, 408, 503, 555];

/// Enum con los subtipos que la factoría puede producir.
///
/// NOTA: la factoría del Python NO implementa builders para `ext_keys`
/// ni `ack` en su `from_network` (solo emite `ETSI020_Message` de
/// error si la validación falla). Reproducimos esa misma cobertura
/// para no divergir del comportamiento referencia.
///
/// Los callers que necesiten parsear cuerpos de `ext_keys` o `ack`
/// deben deserializar directamente a [`super::Etsi020PostExtKeys`] o
/// [`super::Etsi020PostExtKeysAck`].
#[derive(Debug, Clone)]
pub enum Etsi020Built {
    /// Request: `/versions`.
    GetVersions(Etsi020GetVersions),
    /// Request: `/ext_keys/void`.
    PostExtKeysVoid(Etsi020PostExtKeysVoid),
    /// Response: `/versions`.
    VersionContainer(Etsi020VersionContainer),
    /// Cualquier error 4xx/5xx — cuerpo serializado como
    /// `Etsi020Message` (modelo `message` + `details`).
    Error(Etsi020Message),
}

/// Factoría — equivalente a la clase `ETSI020` del Python.
pub struct Etsi020;

impl Etsi020 {
    /// Construye el subtipo apropiado. Devuelve `None` si no se
    /// reconoce o la validación falla (mismo comportamiento que el
    /// Python ante `ValidationError`).
    pub fn from_network(msg: &NetworkMessage) -> Option<Etsi020Built> {
        // Deriva el endpoint del path si no viene explícito.
        let endpoint = msg
            .endpoint
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| derive_endpoint(&msg.path));
        let endpoint = endpoint.as_str();

        if msg.is_response {
            if let Some(code) = msg.status_code {
                if AVAILABLE_ERROR_CODES.contains(&code) {
                    let body = match &msg.data {
                        Some(Value::Object(map)) => {
                            let message = map
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("error")
                                .to_string();
                            let details = map.get("details").and_then(|d| {
                                serde_json::from_value::<Vec<serde_json::Map<String, Value>>>(
                                    d.clone(),
                                )
                                .ok()
                            });
                            Etsi020Message { message, details }
                        }
                        _ => Etsi020Message {
                            message: "error".into(),
                            details: None,
                        },
                    };
                    return Some(Etsi020Built::Error(body));
                }
            }
            return match endpoint {
                "versions" => msg
                    .data
                    .as_ref()
                    .and_then(|d| serde_json::from_value::<Etsi020VersionContainer>(d.clone()).ok())
                    .map(Etsi020Built::VersionContainer),
                _ => None,
            };
        }

        // Request side: igual que Python, solo enrutamos versions y void.
        match endpoint {
            "versions" => Etsi020GetVersions::from_network(msg).map(Etsi020Built::GetVersions),
            "void" => Etsi020PostExtKeysVoid::from_network(msg).map(Etsi020Built::PostExtKeysVoid),

            // ext_keys / ack: cubierto en Python solo como rama de error.
            _ => None,
        }
    }
}

/// Deriva el endpoint inspeccionando el path — equivalente al bloque
/// del Python:
///
/// ```text
/// if '/versions' in path:        endpoint = 'versions'
/// elif '/ext_keys/ack' in path:  endpoint = 'ack'
/// elif '/ext_keys/void' in path: endpoint = 'void'
/// elif '/ext_keys' in path:      endpoint = 'ext_keys'
/// ```
fn derive_endpoint(path: &str) -> String {
    if path.contains("/versions") {
        "versions"
    } else if path.contains("/ext_keys/ack") {
        "ack"
    } else if path.contains("/ext_keys/void") {
        "void"
    } else if path.contains("/ext_keys") {
        "ext_keys"
    } else {
        ""
    }
    .to_string()
}
