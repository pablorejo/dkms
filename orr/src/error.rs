//! ORR error type. The gRPC server maps variants to `tonic::Status`
//! (`Unsupported` → `UNIMPLEMENTED`); `main.rs` is the only place it
//! becomes `anyhow::Error`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrrError {
    #[error("circuit {0} not found")]
    NotFound(String),

    #[error("handshake failed: {0}")]
    Handshake(String),

    #[error("relay failed: {0}")]
    Relay(String),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),

    #[error("aead: {0}")]
    Aead(#[from] common::crypto::aead::AeadError),

    #[error("base64 decode: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Pqc(#[from] common::crypto::pqc::PqcError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, OrrError>;
