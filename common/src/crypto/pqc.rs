//! Post-quantum KEM (ML-KEM, FIPS 203).
//!
//! Backend pure-Rust vía la crate [`ml-kem`] (RustCrypto). No depende de
//! `liboqs`. Soporta los tres parameter sets estándar:
//!
//!   * ML-KEM-512   (security cat 1, equivalente a AES-128)
//!   * ML-KEM-768   (cat 3, equivalente a AES-192)
//!   * ML-KEM-1024  (cat 5, equivalente a AES-256)
//!
//! El equivalente al `Kyber` del Python (`Crypto/key_exchange/PQCKyber/`):
//! mismo API conceptual — `keygen`, `encap(pk)`, `decap(sk, ct)`. La
//! diferencia: el ML-KEM final (FIPS 203) **no es bit-compat** con el
//! "Kyber-Round-3" que `liboqs` exponía cuando se escribió el Python.
//! Para esta migración asumimos que no hace falta interop con los nodos
//! Python (el usuario confirmó que se va a reescribir todo).
//!
//! ### Tamaños de buffer (bytes)
//!
//! | Suite          | public key | secret key | ciphertext | shared secret |
//! |----------------|-----------:|-----------:|-----------:|--------------:|
//! | ML-KEM-512     |        800 |       1632 |        768 |            32 |
//! | ML-KEM-768     |       1184 |       2400 |       1088 |            32 |
//! | ML-KEM-1024    |       1568 |       3168 |       1568 |            32 |
//!
//! El `shared_secret` siempre es 32 bytes — encaja directo como clave
//! de AES-256-GCM ([`crate::crypto::aead`]) en el patrón KEM-DEM.

use ml_kem::array::typenum::Unsigned;
use ml_kem::{
    array::Array,
    kem::{Decapsulate, Encapsulate},
    EncodedSizeUser, KemCore, MlKem1024, MlKem512, MlKem768,
};
use rand::rngs::OsRng;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PqcError {
    #[error("unsupported suite: {0}")]
    UnsupportedSuite(String),
    #[error("bad key/ciphertext length: expected {expected}, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("backend error: {0}")]
    Backend(String),
}

/// Identifiers para los parameter sets soportados. Coinciden con los
/// nombres FIPS 203 (en kebab-case minúscula).
pub mod suite {
    pub const ML_KEM_512: &str = "ml-kem-512";
    pub const ML_KEM_768: &str = "ml-kem-768";
    pub const ML_KEM_1024: &str = "ml-kem-1024";
}

/// Generated KEM key pair (bytes serializados, listos para enviar por la red).
///
/// Nota M-1 (audit): estos son bytes **transitorios**; los consumidores que
/// persisten la `secret` la envuelven en `Zeroizing` en su sitio de
/// almacenamiento (qkc `pending_sk`, orr `ephemeral_sks`, `OrrIdentity`). No se
/// pone `Drop` aquí porque impediría mover los campos fuera del struct (varios
/// callers destructuran `public`/`ciphertext`, que son públicos).
#[derive(Clone, Debug)]
pub struct KemKeypair {
    pub public: Vec<u8>,
    pub secret: Vec<u8>,
    pub suite: String,
}

/// Salida de encapsulación. `ciphertext` es público; el `shared_secret` lo
/// zeroiza el consumidor donde lo persiste (ver nota M-1 en [`KemKeypair`]).
#[derive(Clone, Debug)]
pub struct KemEncap {
    pub ciphertext: Vec<u8>,
    pub shared_secret: Vec<u8>, // 32 bytes (B32)
}

/// Trait dyn-friendly para los KEMs. La indirección permite seleccionar
/// el suite en runtime via [`kem_for`] sin que el caller dependa del
/// parameter set concreto.
pub trait Kem: Send + Sync {
    fn suite(&self) -> &'static str;
    fn public_key_len(&self) -> usize;
    fn secret_key_len(&self) -> usize;
    fn ciphertext_len(&self) -> usize;
    fn keygen(&self) -> Result<KemKeypair, PqcError>;
    fn encap(&self, peer_public: &[u8]) -> Result<KemEncap, PqcError>;
    fn decap(&self, secret: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, PqcError>;
}

/// Selecciona la implementación adecuada a partir del nombre de suite.
/// Devuelve `UnsupportedSuite` para cualquier valor que no esté en
/// [`suite`].
pub fn kem_for(suite: &str) -> Result<Box<dyn Kem>, PqcError> {
    match suite {
        suite::ML_KEM_512 => Ok(Box::new(MlKem512Kem)),
        suite::ML_KEM_768 => Ok(Box::new(MlKem768Kem)),
        suite::ML_KEM_1024 => Ok(Box::new(MlKem1024Kem)),
        other => Err(PqcError::UnsupportedSuite(other.into())),
    }
}

// ── Implementaciones por parameter set ─────────────────────────────────
//
// Cada uno es un struct sin estado: las claves se serializan a `Vec<u8>`
// inmediatamente para que el llamador las maneje como bytes y no
// arrastre tipos genéricos de `ml-kem` por el resto del proyecto.

pub struct MlKem512Kem;
pub struct MlKem768Kem;
pub struct MlKem1024Kem;

macro_rules! impl_kem {
    ($name:ident, $params:ty, $suite:expr) => {
        impl Kem for $name {
            fn suite(&self) -> &'static str { $suite }

            fn public_key_len(&self) -> usize {
                <<<$params as KemCore>::EncapsulationKey as EncodedSizeUser>::EncodedSize as Unsigned>::USIZE
            }
            fn secret_key_len(&self) -> usize {
                <<<$params as KemCore>::DecapsulationKey as EncodedSizeUser>::EncodedSize as Unsigned>::USIZE
            }
            fn ciphertext_len(&self) -> usize {
                <<$params as KemCore>::CiphertextSize as Unsigned>::USIZE
            }

            fn keygen(&self) -> Result<KemKeypair, PqcError> {
                let mut rng = OsRng;
                let (dk, ek) = <$params as KemCore>::generate(&mut rng);
                Ok(KemKeypair {
                    public: ek.as_bytes().to_vec(),
                    secret: dk.as_bytes().to_vec(),
                    suite:  $suite.into(),
                })
            }

            fn encap(&self, peer_public: &[u8]) -> Result<KemEncap, PqcError> {
                let expected = self.public_key_len();
                if peer_public.len() != expected {
                    return Err(PqcError::BadLength { expected, got: peer_public.len() });
                }
                let ek_enc = Array::try_from(peer_public)
                    .map_err(|_| PqcError::Backend("encap: array conversion".into()))?;
                let ek = <$params as KemCore>::EncapsulationKey::from_bytes(&ek_enc);
                let mut rng = OsRng;
                let (ct, ss) = ek
                    .encapsulate(&mut rng)
                    .map_err(|e| PqcError::Backend(format!("encap: {e:?}")))?;
                Ok(KemEncap {
                    ciphertext:    ct.to_vec(),
                    shared_secret: ss.to_vec(),
                })
            }

            fn decap(&self, secret: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, PqcError> {
                let expected_sk = self.secret_key_len();
                if secret.len() != expected_sk {
                    return Err(PqcError::BadLength { expected: expected_sk, got: secret.len() });
                }
                let expected_ct = self.ciphertext_len();
                if ciphertext.len() != expected_ct {
                    return Err(PqcError::BadLength { expected: expected_ct, got: ciphertext.len() });
                }
                let dk_enc = Array::try_from(secret)
                    .map_err(|_| PqcError::Backend("decap: array conversion (sk)".into()))?;
                let dk = <$params as KemCore>::DecapsulationKey::from_bytes(&dk_enc);
                let ct_enc = Array::try_from(ciphertext)
                    .map_err(|_| PqcError::Backend("decap: array conversion (ct)".into()))?;
                let ss = dk
                    .decapsulate(&ct_enc)
                    .map_err(|e| PqcError::Backend(format!("decap: {e:?}")))?;
                Ok(ss.to_vec())
            }
        }
    };
}

impl_kem!(MlKem512Kem, MlKem512, suite::ML_KEM_512);
impl_kem!(MlKem768Kem, MlKem768, suite::ML_KEM_768);
impl_kem!(MlKem1024Kem, MlKem1024, suite::ML_KEM_1024);

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(kem: &dyn Kem) {
        let kp = kem.keygen().expect("keygen");
        assert_eq!(kp.public.len(), kem.public_key_len());
        assert_eq!(kp.secret.len(), kem.secret_key_len());
        assert_eq!(kp.suite, kem.suite());

        let encap = kem.encap(&kp.public).expect("encap");
        assert_eq!(encap.ciphertext.len(), kem.ciphertext_len());
        assert_eq!(encap.shared_secret.len(), 32);

        let ss = kem.decap(&kp.secret, &encap.ciphertext).expect("decap");
        assert_eq!(ss, encap.shared_secret, "shared secrets must match");
    }

    #[test]
    fn ml_kem_512_round_trip() {
        round_trip(&MlKem512Kem);
    }

    #[test]
    fn ml_kem_768_round_trip() {
        round_trip(&MlKem768Kem);
    }

    #[test]
    fn ml_kem_1024_round_trip() {
        round_trip(&MlKem1024Kem);
    }

    #[test]
    fn kem_for_dispatches() {
        for s in [suite::ML_KEM_512, suite::ML_KEM_768, suite::ML_KEM_1024] {
            let k = kem_for(s).unwrap();
            assert_eq!(k.suite(), s);
            let kp = k.keygen().unwrap();
            let enc = k.encap(&kp.public).unwrap();
            let ss = k.decap(&kp.secret, &enc.ciphertext).unwrap();
            assert_eq!(ss, enc.shared_secret);
        }
    }

    #[test]
    fn kem_for_unknown_suite() {
        assert!(matches!(
            kem_for("kyber-classic"),
            Err(PqcError::UnsupportedSuite(_))
        ));
    }

    #[test]
    fn rejects_bad_pubkey_length() {
        let kem = MlKem768Kem;
        let err = kem.encap(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, PqcError::BadLength { .. }));
    }

    #[test]
    fn wrong_ciphertext_fails_decap() {
        let kem = MlKem768Kem;
        let kp = kem.keygen().unwrap();
        // ML-KEM no falla con CT mal formado (implicit rejection): devuelve
        // un secret derivado del Z interno. Lo único que se garantiza es
        // que NO coincide con la encap real con CT distinto.
        let enc = kem.encap(&kp.public).unwrap();
        let mut tampered = enc.ciphertext.clone();
        tampered[0] ^= 0xFF;
        let ss_tampered = kem.decap(&kp.secret, &tampered).unwrap();
        assert_ne!(ss_tampered, enc.shared_secret);
    }
}
