use thiserror::Error;

#[derive(Debug, Error)]
pub enum QkcError {
    #[error("kme: no keys available for peer {0}")]
    NoKeys(String),

    #[error("kme: reservation {0} not found")]
    ReservationNotFound(String),

    #[error("token bucket: link {0} rate-limited")]
    RateLimited(String),

    #[error("routing: no next hop for dest {0}")]
    NoRoute(String),

    #[error("transport: {0}")]
    Transport(String),

    #[error(transparent)]
    Wire(#[from] common::ipc::binary_tcp::WireError),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, QkcError>;
