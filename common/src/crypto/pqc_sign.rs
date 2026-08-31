//! Firma post-cuántica **ML-DSA-65** (FIPS 204, RustCrypto `ml-dsa`).
//!
//! Es la firma hermana del ML-KEM que ya usa el proyecto para KEM. Se usa
//! para autenticar el handshake QKC↔QKC con criptografía **asimétrica
//! post-cuántica** (frames 0x26/0x27), como upgrade del HMAC-PSK de la Fase 5:
//! con firmas cada QKC publica solo su clave de verificación (pública), en vez
//! de compartir un secreto simétrico por enlace.
//!
//! Tamaños ML-DSA-65: semilla de clave privada 32 B, clave de verificación
//! 1952 B, firma 3309 B. La clave privada se serializa como **semilla** de 32 B
//! (de ella se deriva todo el material), lo que la hace cómoda de configurar.
//!
//! API mínima orientada a bytes (como `pqc`): nadie fuera de aquí arrastra los
//! tipos genéricos de `ml-dsa`.

use ml_dsa::{
    signature::{Keypair, Signer, Verifier},
    EncodedSignature, EncodedVerifyingKey, Generate, KeyExport, MlDsa65, Seed, Signature,
    SigningKey, VerifyingKey,
};

/// Nombre de la suite de firma (para atar en el dominio y diagnóstico).
pub const ML_DSA_65: &str = "ml-dsa-65";

/// Longitudes en bytes (ML-DSA-65).
pub const SEED_LEN: usize = 32;
pub const VERIFYING_KEY_LEN: usize = 1952;
pub const SIGNATURE_LEN: usize = 3309;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SignError {
    #[error("bad key/signature length")]
    BadLength,
    #[error("invalid signature")]
    Invalid,
    #[error("the key is not an ML-DSA-65 PKCS#8 (seed form)")]
    NotMlDsa,
}

/// Clave de firma ML-DSA-65 cargada de un PKCS#8 — la del certificado de
/// nodo. Con ella el anuncio de pubkey del ORR queda atado al cert (y por
/// tanto a la CA de red), en vez de a una semilla aparte que había que
/// repartir a mano como `peer_verify_keys`.
pub struct MlDsa65Signer {
    sk: SigningKey<MlDsa65>,
}

impl MlDsa65Signer {
    /// Desde el DER PKCS#8 en forma semilla (la que emite `gen-certs.sh`).
    pub fn from_pkcs8_der(der: &[u8]) -> Result<Self, SignError> {
        use ml_dsa::pkcs8::DecodePrivateKey;
        SigningKey::<MlDsa65>::from_pkcs8_der(der)
            .map(|sk| Self { sk })
            .map_err(|_| SignError::NotMlDsa)
    }

    /// Desde el PEM de `tls.key_path`. Una clave RSA/ECDSA devuelve
    /// [`SignError::NotMlDsa`].
    pub fn from_pkcs8_pem(pem: &[u8]) -> Result<Self, SignError> {
        use rustls::pki_types::{pem::PemObject, PrivateKeyDer};
        match PrivateKeyDer::from_pem_slice(pem) {
            Ok(PrivateKeyDer::Pkcs8(k)) => Self::from_pkcs8_der(k.secret_pkcs8_der()),
            _ => Err(SignError::NotMlDsa),
        }
    }

    /// Clave de verificación (1952 B): la misma que lleva el SPKI del cert.
    pub fn verifying_key(&self) -> Vec<u8> {
        self.sk.verifying_key().encode().to_vec()
    }

    pub fn sign(&self, msg: &[u8]) -> Vec<u8> {
        self.sk.sign(msg).encode().to_vec()
    }
}

/// Par de firma ML-DSA serializado a bytes.
pub struct SignKeypair {
    /// Semilla de la clave privada (32 B). De aquí se deriva el resto.
    pub secret_seed: zeroize::Zeroizing<Vec<u8>>,
    /// Clave pública de verificación (1952 B) — se publica a los peers.
    pub verifying_key: Vec<u8>,
}

/// Genera un par ML-DSA-65 fresco.
pub fn keygen() -> SignKeypair {
    let sk = SigningKey::<MlDsa65>::generate();
    let vk = sk.verifying_key();
    SignKeypair {
        secret_seed: zeroize::Zeroizing::new(sk.to_bytes().to_vec()),
        verifying_key: vk.encode().to_vec(),
    }
}

fn signing_key_from_seed(seed: &[u8]) -> Result<SigningKey<MlDsa65>, SignError> {
    let seed = Seed::try_from(seed).map_err(|_| SignError::BadLength)?;
    Ok(SigningKey::<MlDsa65>::from_seed(&seed))
}

/// Firma `msg` con la semilla de clave privada. Devuelve la firma (3309 B).
pub fn sign(secret_seed: &[u8], msg: &[u8]) -> Result<Vec<u8>, SignError> {
    let sk = signing_key_from_seed(secret_seed)?;
    Ok(sk.sign(msg).encode().to_vec())
}

/// Verifica `sig` sobre `msg` con la clave pública de verificación.
pub fn verify(verifying_key: &[u8], msg: &[u8], sig: &[u8]) -> Result<(), SignError> {
    let vk_enc = EncodedVerifyingKey::<MlDsa65>::try_from(verifying_key)
        .map_err(|_| SignError::BadLength)?;
    let vk = VerifyingKey::<MlDsa65>::decode(&vk_enc);
    let sig_enc = EncodedSignature::<MlDsa65>::try_from(sig).map_err(|_| SignError::BadLength)?;
    let signature = Signature::<MlDsa65>::decode(&sig_enc).ok_or(SignError::Invalid)?;
    vk.verify(msg, &signature).map_err(|_| SignError::Invalid)
}

// ── Firma del handshake QKC↔QKC ────────────────────────────────────────────
//
// Mismo mensaje canónico que el HMAC de la Fase 5
// (`common::crypto::link_mac`): ata época, identidades, blob, suite y
// key_size. Así el modo `sign` (ML-DSA) es el equivalente asimétrico del modo
// PSK, intercambiable a nivel de qué se protege.

/// Construye el mensaje canónico del handshake que se firma/verifica.
/// `tag` es una etiqueta de dominio (`link_mac::TAG_INIT`/`TAG_RESP`).
fn canonical_msg(
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
) -> Vec<u8> {
    let s = suite_id.as_bytes();
    let mut m = Vec::with_capacity(tag.len() + 12 + 4 + blob.len() + s.len() + 4);
    m.extend_from_slice(tag);
    m.extend_from_slice(&epoch.to_be_bytes());
    m.extend_from_slice(&sender_id.to_be_bytes());
    m.extend_from_slice(&receiver_id.to_be_bytes());
    m.extend_from_slice(&(blob.len() as u16).to_be_bytes());
    m.extend_from_slice(blob);
    m.extend_from_slice(&(s.len() as u16).to_be_bytes());
    m.extend_from_slice(s);
    m.extend_from_slice(&key_size_bits.to_be_bytes());
    m
}

/// Firma un mensaje del handshake con la semilla ML-DSA de este QKC.
#[allow(clippy::too_many_arguments)]
pub fn sign_handshake(
    secret_seed: &[u8],
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
) -> Result<Vec<u8>, SignError> {
    let msg = canonical_msg(
        tag,
        epoch,
        sender_id,
        receiver_id,
        blob,
        suite_id,
        key_size_bits,
    );
    sign(secret_seed, &msg)
}

/// Verifica la firma ML-DSA de un mensaje del handshake con la clave pública
/// del peer.
#[allow(clippy::too_many_arguments)]
pub fn verify_handshake(
    verifying_key: &[u8],
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
    sig: &[u8],
) -> Result<(), SignError> {
    let msg = canonical_msg(
        tag,
        epoch,
        sender_id,
        receiver_id,
        blob,
        suite_id,
        key_size_bits,
    );
    verify(verifying_key, &msg, sig)
}

// ── Firma del anuncio de pubkey del ORR (bootstrap) ────────────────────────

/// Etiqueta de dominio del anuncio de pubkey de un ORR.
const TAG_ORR_PUBKEY: &[u8] = b"ORRPUBKEY";

fn announcement_msg(orr_id: &str, suite: &str, public_key: &[u8]) -> Vec<u8> {
    let id = orr_id.as_bytes();
    let s = suite.as_bytes();
    let mut m =
        Vec::with_capacity(TAG_ORR_PUBKEY.len() + 6 + id.len() + s.len() + public_key.len());
    m.extend_from_slice(TAG_ORR_PUBKEY);
    m.extend_from_slice(&(id.len() as u16).to_be_bytes());
    m.extend_from_slice(id);
    m.extend_from_slice(&(s.len() as u16).to_be_bytes());
    m.extend_from_slice(s);
    m.extend_from_slice(&(public_key.len() as u16).to_be_bytes());
    m.extend_from_slice(public_key);
    m
}

/// Firma un mensaje del handshake de enlace con la clave del **certificado de
/// nodo** (`MlDsa65Signer`), en vez de con una semilla cruda. El mensaje
/// canónico es idéntico al de [`sign_handshake`], así que un peer lo verifica
/// con [`verify_handshake`] usando la clave pública que salga de
/// `cert_identity::verify_node_cert`.
#[allow(clippy::too_many_arguments)]
pub fn sign_handshake_with(
    signer: &MlDsa65Signer,
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
) -> Vec<u8> {
    let msg = canonical_msg(
        tag,
        epoch,
        sender_id,
        receiver_id,
        blob,
        suite_id,
        key_size_bits,
    );
    signer.sign(&msg)
}

/// Firma el anuncio `(orr_id, suite, public_key)` con la clave del cert de nodo.
pub fn sign_orr_pubkey_with(
    signer: &MlDsa65Signer,
    orr_id: &str,
    suite: &str,
    public_key: &[u8],
) -> Vec<u8> {
    signer.sign(&announcement_msg(orr_id, suite, public_key))
}

/// Firma el anuncio `(orr_id, suite, public_key)` de un ORR (bootstrap PQC).
pub fn sign_orr_pubkey(
    secret_seed: &[u8],
    orr_id: &str,
    suite: &str,
    public_key: &[u8],
) -> Result<Vec<u8>, SignError> {
    sign(secret_seed, &announcement_msg(orr_id, suite, public_key))
}

/// Verifica la firma del anuncio de pubkey de un ORR con su clave pública.
pub fn verify_orr_pubkey(
    verifying_key: &[u8],
    orr_id: &str,
    suite: &str,
    public_key: &[u8],
    sig: &[u8],
) -> Result<(), SignError> {
    verify(
        verifying_key,
        &announcement_msg(orr_id, suite, public_key),
        sig,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keygen_sizes() {
        let kp = keygen();
        assert_eq!(kp.secret_seed.len(), SEED_LEN);
        assert_eq!(kp.verifying_key.len(), VERIFYING_KEY_LEN);
    }

    #[test]
    fn sign_verify_round_trip() {
        let kp = keygen();
        let msg = b"handshake bytes to authenticate";
        let sig = sign(&kp.secret_seed, msg).unwrap();
        assert_eq!(sig.len(), SIGNATURE_LEN);
        assert!(verify(&kp.verifying_key, msg, &sig).is_ok());
    }

    #[test]
    fn rejects_tampered_message_and_wrong_key() {
        let kp = keygen();
        let other = keygen();
        let msg = b"original message";
        let sig = sign(&kp.secret_seed, msg).unwrap();
        // Mensaje alterado.
        assert_eq!(
            verify(&kp.verifying_key, b"other message", &sig),
            Err(SignError::Invalid)
        );
        // Clave de verificación equivocada.
        assert_eq!(
            verify(&other.verifying_key, msg, &sig),
            Err(SignError::Invalid)
        );
    }

    #[test]
    fn bad_lengths_rejected_cleanly() {
        assert_eq!(sign(&[0u8; 8], b"m"), Err(SignError::BadLength));
        assert_eq!(
            verify(&[0u8; 8], b"m", &[0u8; SIGNATURE_LEN]),
            Err(SignError::BadLength)
        );
    }

    #[test]
    fn orr_pubkey_announcement_sign_verify() {
        let kp = keygen();
        let pk = vec![0x11; 1184];
        let sig = sign_orr_pubkey(&kp.secret_seed, "orr_1", "ml-kem-768", &pk).unwrap();
        assert!(verify_orr_pubkey(&kp.verifying_key, "orr_1", "ml-kem-768", &pk, &sig).is_ok());
        // orr_id distinto (un MITM anunciando otra identidad) → inválido.
        assert_eq!(
            verify_orr_pubkey(&kp.verifying_key, "orr_2", "ml-kem-768", &pk, &sig),
            Err(SignError::Invalid),
        );
        // pubkey sustituida (el ataque MITM real) → inválido.
        assert_eq!(
            verify_orr_pubkey(
                &kp.verifying_key,
                "orr_1",
                "ml-kem-768",
                &[0x22; 1184],
                &sig
            ),
            Err(SignError::Invalid),
        );
    }

    #[test]
    fn handshake_sign_verify_binds_fields() {
        use crate::crypto::link_mac::{TAG_INIT, TAG_RESP};
        let kp = keygen();
        let blob = vec![0xAB; 1184];
        let sig = sign_handshake(
            &kp.secret_seed,
            TAG_INIT,
            7,
            1,
            2,
            &blob,
            "ml-kem-768",
            1024,
        )
        .unwrap();
        // Correcto.
        assert!(verify_handshake(
            &kp.verifying_key,
            TAG_INIT,
            7,
            1,
            2,
            &blob,
            "ml-kem-768",
            1024,
            &sig
        )
        .is_ok());
        // Cualquier campo distinto invalida (tag, época, ids, suite, key_size).
        for bad in [
            verify_handshake(
                &kp.verifying_key,
                TAG_RESP,
                7,
                1,
                2,
                &blob,
                "ml-kem-768",
                1024,
                &sig,
            ),
            verify_handshake(
                &kp.verifying_key,
                TAG_INIT,
                8,
                1,
                2,
                &blob,
                "ml-kem-768",
                1024,
                &sig,
            ),
            verify_handshake(
                &kp.verifying_key,
                TAG_INIT,
                7,
                2,
                1,
                &blob,
                "ml-kem-768",
                1024,
                &sig,
            ),
            verify_handshake(
                &kp.verifying_key,
                TAG_INIT,
                7,
                1,
                2,
                &blob,
                "ml-kem-512",
                1024,
                &sig,
            ),
            verify_handshake(
                &kp.verifying_key,
                TAG_INIT,
                7,
                1,
                2,
                &blob,
                "ml-kem-768",
                256,
                &sig,
            ),
        ] {
            assert_eq!(bad, Err(SignError::Invalid));
        }
    }
}
