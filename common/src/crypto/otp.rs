//! One-time-pad helpers.
//!
//! QKD key material is consumed as an OTP for hop-to-hop confidentiality.
//! These helpers do the bookkeeping and the XOR, but they do NOT manage
//! key freshness — that is the caller's responsibility.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum OtpError {
    #[error("key shorter than plaintext: key={key} pt={pt}")]
    KeyTooShort { key: usize, pt: usize },
}

/// XOR `plaintext` with `key` in place. Returns the number of bytes
/// processed.
pub fn xor_in_place(plaintext: &mut [u8], key: &[u8]) -> Result<usize, OtpError> {
    if key.len() < plaintext.len() {
        return Err(OtpError::KeyTooShort { key: key.len(), pt: plaintext.len() });
    }
    for (b, k) in plaintext.iter_mut().zip(key.iter()) {
        *b ^= *k;
    }
    Ok(plaintext.len())
}

/// Allocating variant.
pub fn xor(plaintext: &[u8], key: &[u8]) -> Result<Vec<u8>, OtpError> {
    if key.len() < plaintext.len() {
        return Err(OtpError::KeyTooShort { key: key.len(), pt: plaintext.len() });
    }
    Ok(plaintext.iter().zip(key.iter()).map(|(p, k)| p ^ k).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_round_trips() {
        let pt = b"hello world".to_vec();
        let k  = b"01234567890abcdef".to_vec();
        let ct = xor(&pt, &k).unwrap();
        let pt2 = xor(&ct, &k).unwrap();
        assert_eq!(pt, pt2);
    }
}
