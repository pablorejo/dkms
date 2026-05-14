//! Post-quantum cryptography facade.
//!
//! Wraps whichever PQC backend we end up using (oqs, ml-kem from RustCrypto,
//! etc.). Today this is a thin placeholder. The shape mirrors the Python
//! `liboqs-python` API surface so callers don't have to change when we wire
//! in a real implementation.
//!
//! TODO: implement against `oqs` (or pure-Rust `pqcrypto-*`) once we pick a
//! backend.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PqcError {
    #[error("unsupported suite: {0}")]
    UnsupportedSuite(String),
    #[error("backend error: {0}")]
    Backend(String),
}

/// Identifiers for supported KEM/SIG suites. Keep them as strings on the
/// wire (see `proto/orr.proto:OpenCircuitRequest.pqc_suite`) so we can add
/// new ones without breaking the schema.
pub mod suite {
    pub const KYBER1024_DILITHIUM5: &str = "kyber1024+dilithium5";
    pub const KYBER768_DILITHIUM3:  &str = "kyber768+dilithium3";
}

/// Generated KEM key pair.
pub struct KemKeypair {
    pub public:  Vec<u8>,
    pub secret:  Vec<u8>,
    pub suite:   String,
}

/// Encapsulation output.
pub struct KemEncap {
    pub ciphertext: Vec<u8>,
    pub shared_secret: Vec<u8>,
}

pub trait Kem {
    fn keygen(&self) -> Result<KemKeypair, PqcError>;
    fn encap(&self, peer_public: &[u8]) -> Result<KemEncap, PqcError>;
    fn decap(&self, secret: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, PqcError>;
}

/// Look up a Kem implementation by suite string.
pub fn kem_for(_suite: &str) -> Result<Box<dyn Kem + Send + Sync>, PqcError> {
    Err(PqcError::UnsupportedSuite("not implemented yet".into()))
}
