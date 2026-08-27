use thiserror::Error;

#[derive(Debug, Error)]
pub enum QkcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("bad request: {0}")]
    BadRequest(String),

    #[error("no route to dest {0}")]
    NoRoute(u32),

    #[error("no link/quditto for neighbor {0}")]
    UnknownNeighbor(u32),

    /// El frame no pasó la autenticación del enlace: MAC inválido, replay, o
    /// llegó en claro a un enlace en `frame_auth = require`.
    #[error("frame auth: {0}")]
    FrameAuth(#[from] crate::frame_auth::FrameAuthError),

    #[error("quditto returned not-enough-keys: requested {requested}, got {received}")]
    NotEnoughKeys { requested: u32, received: u32 },

    #[error("timeout waiting {what} ({missing} missing after {ms} ms)")]
    KeyWaitTimeout {
        what: &'static str,
        missing: usize,
        ms: u64,
    },

    #[error("quditto: {0}")]
    Quditto(String),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("wire: {0}")]
    Wire(#[from] wire::WireError),

    #[error(transparent)]
    Etsi(#[from] etsi::EtsiError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, QkcError>;
