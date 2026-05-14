//! Errors that can cross crate boundaries.
//!
//! Each module also defines its own `Error` enum for module-internal failures;
//! `CommonError` is the lingua franca when something has to bubble up through
//! the shared library.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum CommonError {
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("tonic transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("tonic status: {0}")]
    Status(#[from] tonic::Status),

    #[error("tls error: {0}")]
    Tls(String),

    #[error("invalid argument: {0}")]
    InvalidArg(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("unavailable: {0}")]
    Unavailable(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl CommonError {
    pub fn invalid_arg(msg: impl Into<String>) -> Self {
        Self::InvalidArg(msg.into())
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::NotFound(msg.into())
    }
    pub fn unavailable(msg: impl Into<String>) -> Self {
        Self::Unavailable(msg.into())
    }
}

/// Map our error into a tonic gRPC `Status` so handlers can `?` it.
impl From<CommonError> for tonic::Status {
    fn from(e: CommonError) -> Self {
        use tonic::Code;
        match e {
            CommonError::InvalidArg(m) => tonic::Status::new(Code::InvalidArgument, m),
            CommonError::NotFound(m) => tonic::Status::new(Code::NotFound, m),
            CommonError::Unavailable(m) => tonic::Status::new(Code::Unavailable, m),
            CommonError::Status(s) => s,
            other => tonic::Status::new(Code::Internal, other.to_string()),
        }
    }
}
