//! Errores del crate quditto.
//!
//! Sin dependencia de `common::error::CommonError` — quditto es ahora
//! un binario autocontenido (HTTP/axum + crate `etsi`), no consume
//! `common/` ni gRPC.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum QudittoError {
    #[error("buffer empty: not enough fresh keys (requested {requested}, available {available})")]
    NotEnoughKeys { requested: u32, available: u64 },

    #[error("key_id {0} not found in delivered map")]
    UnknownKeyId(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error(transparent)]
    Etsi(#[from] etsi::EtsiError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, QudittoError>;
