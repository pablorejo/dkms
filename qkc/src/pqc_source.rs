//! Fuente de claves PQC para un enlace QKC↔QKC (canal `link_type = "pqc"`).
//!
//! Imita el feed de claves del quditto (QKD) sin red ni quditto: los dos
//! QKC del enlace comparten un secreto de 32 B acordado por ML-KEM (ver
//! [`crate::pqc_handshake`]) y derivan de él, de forma **determinista**, el
//! material OTP de cada clave:
//!
//! ```text
//! K = HKDF-SHA256(salt = b"qkc.pqc.v1",
//!                 ikm  = secret_32B,
//!                 info = key_id ‖ u32_be(len) ‖ u32_be(chunk_idx),
//!                 L    = key_size_bits/8)
//! ```
//!
//! El lado emisor (`enc_keys`) elige `key_id`s UUID v4 y deriva su material;
//! el `enc_refill_loop` los anuncia al peer por `FRAME_KEY_IDS_NOTIFY` igual
//! que en QKD. El lado receptor (`dec_keys`) deriva **el mismo** material
//! localmente a partir del mismo `(secreto, key_id)` — sin HTTP, sin 404.
//!
//! El salt `b"qkc.pqc.v1"` es distinto del del onion de ORR
//! (`b"orr.onion.v1"`) para que las dos derivaciones nunca coincidan; este
//! módulo NO depende de `orr`, solo de `common::crypto::pqc` (ML-KEM) vía el
//! handshake y de `hkdf`/`sha2` para el KDF.

use std::time::{Duration, Instant};

use hkdf::Hkdf;
use sha2::Sha256;
use tokio::sync::watch;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    error::{QkcError, Result},
    kme::{KeySource, OtpKey},
};

/// Secreto compartido de 32 B detrás de un `watch`; `None` hasta que el
/// handshake ML-KEM termina. El `Sender` lo posee [`crate::pqc_handshake`].
pub type SecretWatch = watch::Receiver<Option<Zeroizing<[u8; 32]>>>;

const QKC_PQC_SALT: &[u8] = b"qkc.pqc.v1";
/// Techo de salida de un solo `HKDF-SHA256::expand` (RFC 5869: 255·32 B).
const HKDF_MAX_OUTPUT: usize = 255 * 32;
/// Cuánto espera `enc/dec_keys` a que el handshake complete antes de
/// devolver timeout. El `enc_refill_loop` reintenta tras su backoff de
/// 100 ms, así que esto solo acota la duración de cada intento bloqueado.
const SECRET_WAIT: Duration = Duration::from_secs(10);

/// Deriva `len` bytes deterministas a partir de `(secret, key_id)` con
/// HKDF-SHA256. Soporta `len` arbitrario vía chunking (cada chunk usa un
/// `chunk_idx` distinto en el `info`). Ambos extremos del enlace derivan
/// bytes idénticos para el mismo `(secret, key_id, len)`.
pub fn derive_material(secret: &[u8; 32], key_id: &[u8; 16], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(QKC_PQC_SALT), secret);
    let mut okm = vec![0u8; len];
    let mut info = [0u8; 16 + 4 + 4]; // key_id ‖ u32_be(len) ‖ u32_be(chunk_idx)
    info[..16].copy_from_slice(key_id);
    info[16..20].copy_from_slice(&(len as u32).to_be_bytes());

    let mut off = 0usize;
    let mut chunk_idx: u32 = 0;
    while off < len {
        let take = (len - off).min(HKDF_MAX_OUTPUT);
        info[20..24].copy_from_slice(&chunk_idx.to_be_bytes());
        hk.expand(&info, &mut okm[off..off + take])
            .expect("HKDF-SHA256 expand within 8160 B chunk");
        off += take;
        chunk_idx = chunk_idx
            .checked_add(1)
            .expect("len overflows chunk_idx u32");
    }
    okm
}

/// [`KeySource`] de un enlace PQC.
pub struct PqcKeySource {
    /// Longitud del material por clave (= `key_size_bits / 8`).
    key_bytes: usize,
    /// Secreto compartido (lo rellena el handshake).
    secret_rx: SecretWatch,
}

impl PqcKeySource {
    pub fn new(key_size_bits: u32, secret_rx: SecretWatch) -> Self {
        Self {
            key_bytes: (key_size_bits / 8) as usize,
            secret_rx,
        }
    }

    /// Espera (con deadline) a que el handshake publique el secreto.
    async fn await_secret(&self) -> Result<Zeroizing<[u8; 32]>> {
        // Fast path: ya está.
        if let Some(s) = self.secret_rx.borrow().as_ref() {
            return Ok(s.clone());
        }
        let mut rx = self.secret_rx.clone();
        let deadline = Instant::now() + SECRET_WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_err());
            }
            tokio::select! {
                res = rx.changed() => {
                    if res.is_err() {
                        // El Sender se cayó (no debería mientras viva el enlace).
                        return Err(QkcError::KeyWaitTimeout {
                            what: "pqc-handshake-closed",
                            missing: 1,
                            ms: 0,
                        });
                    }
                    if let Some(s) = rx.borrow_and_update().as_ref() {
                        return Ok(s.clone());
                    }
                }
                _ = tokio::time::sleep(remaining) => return Err(timeout_err()),
            }
        }
    }
}

fn timeout_err() -> QkcError {
    QkcError::KeyWaitTimeout {
        what: "pqc-handshake",
        missing: 1,
        ms: SECRET_WAIT.as_millis() as u64,
    }
}

#[async_trait::async_trait]
impl KeySource for PqcKeySource {
    async fn enc_keys(&self, number: u32) -> Result<Vec<OtpKey>> {
        if number == 0 {
            return Ok(vec![]);
        }
        let secret = self.await_secret().await?;
        let mut out = Vec::with_capacity(number as usize);
        for _ in 0..number {
            let key_id = Uuid::new_v4();
            let material = derive_material(&secret, key_id.as_bytes(), self.key_bytes);
            out.push(OtpKey { key_id, material });
        }
        Ok(out)
    }

    async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let secret = self.await_secret().await?;
        Ok(ids
            .iter()
            .map(|id| OtpKey {
                key_id: *id,
                material: derive_material(&secret, id.as_bytes(), self.key_bytes),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::crypto::pqc::{kem_for, suite};

    #[test]
    fn derive_material_deterministic_and_sized() {
        let secret = [7u8; 32];
        let id = [3u8; 16];
        let a = derive_material(&secret, &id, 32);
        let b = derive_material(&secret, &id, 32);
        assert_eq!(a, b, "same (secret,key_id) must derive identical bytes");
        assert_eq!(a.len(), 32);
        // Distinto key_id ⇒ keystream distinto.
        assert_ne!(derive_material(&secret, &[4u8; 16], 32), a);
        // Distinto secreto ⇒ keystream distinto.
        assert_ne!(derive_material(&[8u8; 32], &id, 32), a);
        // len arbitrario > 8160 B (fuerza chunking) sin pánico.
        assert_eq!(derive_material(&secret, &id, 9000).len(), 9000);
    }

    /// Gate de corrección principal: tras un round-trip ML-KEM real, dos
    /// `PqcKeySource` (uno por extremo) derivan material byte-idéntico para
    /// los mismos `key_id` — exactamente lo que necesita el OTP del relay.
    #[tokio::test]
    async fn both_ends_derive_identical_material() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        let encap = kem.encap(&kp.public).unwrap();
        let ss_peer = kem.decap(&kp.secret, &encap.ciphertext).unwrap();
        assert_eq!(encap.shared_secret, ss_peer, "ML-KEM secrets must match");

        let to32 = |v: &[u8]| {
            let mut a = [0u8; 32];
            a.copy_from_slice(v);
            Zeroizing::new(a)
        };
        let (_tx_a, rx_a) = watch::channel(Some(to32(&encap.shared_secret)));
        let (_tx_b, rx_b) = watch::channel(Some(to32(&ss_peer)));

        let side_a = PqcKeySource::new(256, rx_a); // emisor
        let side_b = PqcKeySource::new(256, rx_b); // receptor

        let enc = side_a.enc_keys(4).await.unwrap();
        let ids: Vec<Uuid> = enc.iter().map(|k| k.key_id).collect();
        let dec = side_b.dec_keys(&ids).await.unwrap();

        assert_eq!(enc.len(), 4);
        for (e, d) in enc.iter().zip(dec.iter()) {
            assert_eq!(e.key_id, d.key_id);
            assert_eq!(e.material.len(), 32);
            assert_eq!(e.material, d.material, "enc/dec material must be identical");
        }
    }
}
