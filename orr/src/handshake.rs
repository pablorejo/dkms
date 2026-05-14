//! Per-hop PQC handshake.
//!
//! When a new circuit is opened, every adjacent pair on the path performs
//! a KEM handshake to derive a shared session key. We don't pin a backend
//! here — see [`common::crypto::pqc`].

use common::crypto::pqc::PqcError;

use crate::error::{OrrError, Result};

pub struct HandshakeResult {
    pub session_key: Vec<u8>,
    pub session_id:  Vec<u8>,
}

/// Initiate side of the handshake: encapsulate against the peer's KEM
/// public key.
pub fn initiate(_suite: &str, _peer_public: &[u8]) -> Result<HandshakeResult> {
    Err(OrrError::Handshake(format!("not implemented: {}", PqcError::UnsupportedSuite("init".into()))))
}

/// Responder side: decapsulate and derive the same session key.
pub fn respond(_suite: &str, _secret: &[u8], _ciphertext: &[u8]) -> Result<HandshakeResult> {
    Err(OrrError::Handshake(format!("not implemented: {}", PqcError::UnsupportedSuite("resp".into()))))
}
