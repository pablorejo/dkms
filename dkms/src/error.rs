//! Tipos de error del DKMS.
//!
//! Crucial para los handlers ETSI: el mapping a HTTP status se hace en
//! [`crate::etsi_http`] desde estas variantes (no decidimos status aquí
//! para mantener el módulo libre de dependencias HTTP).

use thiserror::Error;

use common::ids::SaeId;

#[derive(Debug, Error)]
pub enum DkmsError {
    // ─── ETSI 014 (norte) ───────────────────────────────────────────────
    #[error("unauthenticated: missing or invalid client certificate")]
    Unauthenticated,

    #[error("forbidden: sae {0} cannot act on behalf of {1}")]
    Forbidden(SaeId, SaeId),

    #[error("unknown sae: {0}")]
    UnknownSae(SaeId),

    #[error("rate-limited: sae {sae} requested {requested} tokens, {available} available")]
    RateLimited { sae: SaeId, requested: u64, available: u64 },

    #[error("invalid request: {0}")]
    BadRequest(String),

    #[error("key not found or not authorized for sae {sae}")]
    KeyNotAuthorized { sae: SaeId },

    #[error("key expired")]
    KeyExpired,

    // ─── ETSI 020 (este/oeste) ──────────────────────────────────────────
    #[error("peer dkms {peer} unreachable: {source}")]
    PeerUnreachable { peer: String, source: anyhow::Error },

    #[error("peer dkms {peer} rejected: HTTP {status} — {body}")]
    PeerRejected { peer: String, status: u16, body: String },

    #[error("peer dkms {peer} did not ack within timeout")]
    PeerAckTimeout { peer: String },

    #[error("peer dkms {peer}: orr transport configured but {missing}")]
    PeerOrrMisconfig { peer: String, missing: &'static str },

    #[error("peer dkms {peer}: orr send_message failed: {source}")]
    OrrSendFailed { peer: String, source: anyhow::Error },

    #[error("transport buffer empty for peer {peer}")]
    TransportBufferEmpty { peer: String },

    #[error("transport key {key_id} not in buffer_dec[{peer}]")]
    TransportKeyMissing { peer: String, key_id: String },

    // ─── Sur (gRPC) ────────────────────────────────────────────────────
    #[error("sdn unreachable: {0}")]
    SdnUnreachable(#[source] anyhow::Error),

    #[error("qkc unreachable: {0}")]
    QkcUnreachable(#[source] anyhow::Error),

    #[error("sae binding lookup failed for {0}")]
    SaeBindingLookupFailed(SaeId),

    // ─── Infraestructura ───────────────────────────────────────────────
    #[error(transparent)]
    Tls(#[from] common::tls::TlsError),

    #[error("crypto: {0}")]
    Crypto(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Common(#[from] common::error::CommonError),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, DkmsError>;
