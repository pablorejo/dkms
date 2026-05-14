use thiserror::Error;

#[derive(Debug, Error)]
pub enum DkmsError {
    #[error("sae {0} not found")]
    SaeNotFound(String),

    #[error("buffer empty for ({0}, {1})")]
    BufferEmpty(String, String),

    #[error("rate-limited: sae {0}")]
    RateLimited(String),

    #[error("upstream unavailable: {0}")]
    Upstream(String),

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Tls(#[from] common::tls::TlsError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, DkmsError>;
