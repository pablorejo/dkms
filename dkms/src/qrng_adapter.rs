//! Pulls fresh key material from a local QRNG or quditto.
//!
//! Two backends:
//!  * `random` — `rand::rngs::OsRng` (dev / fallback).
//!  * `quditto` — gRPC `QudittoControl::ReadKeys` (production).

use base64::Engine;
use rand::RngCore;

use crate::{config::DkmsConfig, error::Result};

pub struct QrngAdapter {
    pub mode: Mode,
}

pub enum Mode {
    Os,
    Quditto(String /* url */),
}

impl QrngAdapter {
    pub fn from_config(cfg: &DkmsConfig) -> Self {
        match cfg.qrng_url.as_deref() {
            Some(url) if !url.is_empty() => Self { mode: Mode::Quditto(url.to_owned()) },
            _ => Self { mode: Mode::Os },
        }
    }

    /// Return `count` keys of `size_bits` bits each, base64-encoded for
    /// direct ETSI 014/020 response use.
    pub async fn read_keys_b64(&self, count: u32, size_bits: u32) -> Result<Vec<String>> {
        let bytes_per_key = (size_bits.div_ceil(8)) as usize;
        match &self.mode {
            Mode::Os => {
                let mut rng = rand::rngs::OsRng;
                let mut out = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    let mut buf = vec![0u8; bytes_per_key];
                    rng.fill_bytes(&mut buf);
                    out.push(base64::engine::general_purpose::STANDARD.encode(&buf));
                }
                Ok(out)
            }
            Mode::Quditto(_url) => {
                // TODO: implement via `common::proto::quditto::v1`.
                Ok(vec![])
            }
        }
    }
}
