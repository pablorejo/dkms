//! Wraps/unwraps the QKC payload with OTP keys taken from the local KME.
//!
//! The KME hands out `Key` blobs and this engine just consumes them — it
//! doesn't manage freshness or replenishment. That's [`super::kme::Kme`]'s
//! job.

use common::crypto::otp;

use crate::{error::QkcError, kme::Key};

pub struct CryptoEngine;

impl CryptoEngine {
    pub fn new() -> Self {
        Self
    }

    /// Encrypt `plaintext` using one or more OTP keys concatenated to cover
    /// its length. Returns the ciphertext.
    pub fn encrypt(&self, plaintext: &[u8], keys: &[Key]) -> Result<Vec<u8>, QkcError> {
        let key_material: Vec<u8> = keys.iter().flat_map(|k| k.bytes.iter().copied()).collect();
        otp::xor(plaintext, &key_material).map_err(|e| QkcError::Other(e.into()))
    }

    /// Decrypt — same operation as `encrypt` for OTP.
    pub fn decrypt(&self, ciphertext: &[u8], keys: &[Key]) -> Result<Vec<u8>, QkcError> {
        self.encrypt(ciphertext, keys)
    }
}

impl Default for CryptoEngine {
    fn default() -> Self {
        Self::new()
    }
}
