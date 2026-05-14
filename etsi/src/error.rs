//! Errores específicos del crate `etsi`.
//!
//! Las validaciones (campos requeridos, longitudes mínimas, rangos
//! `ge=0`, `gt=0`) emiten [`EtsiError::Validation`]. La factoría
//! `from_network` devuelve `Option<...>` siguiendo el patrón del Python
//! — un mensaje inválido produce `None`, no un error tipado.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum EtsiError {
    #[error("validation: {0}")]
    Validation(String),

    #[error("invalid base64: {0}")]
    Base64(String),

    #[error("invalid uuid: {0}")]
    Uuid(String),

    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid access method '{0}': must be one of {1:?}")]
    InvalidAccessMethod(String, Vec<String>),
}

pub type Result<T> = std::result::Result<T, EtsiError>;
