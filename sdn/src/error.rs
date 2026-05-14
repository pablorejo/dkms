use thiserror::Error;

#[derive(Debug, Error)]
pub enum SdnError {
    #[error("no path: {0} -> {1}")]
    NoPath(String, String),

    #[error("unknown node: {0}")]
    UnknownNode(String),

    #[error("unknown link: {0}")]
    UnknownLink(String),

    #[error("admission denied: {0}")]
    AdmissionDenied(String),

    #[error("topology: {0}")]
    Topology(String),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, SdnError>;
