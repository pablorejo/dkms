use thiserror::Error;

#[derive(Debug, Error)]
pub enum OrrError {
    #[error("circuit {0} not found")]
    NotFound(String),

    #[error("handshake failed: {0}")]
    Handshake(String),

    #[error("relay failed: {0}")]
    Relay(String),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Pqc(#[from] common::crypto::pqc::PqcError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, OrrError>;
