//! AEAD simétrica (AES-256-GCM).
//!
//! Equivalente al `Crypto/encryption/AEScodec.py` del Python: cifrado
//! autenticado con clave de 32 bytes y nonce de 96 bits aleatorio.
//! Nonce aleatorio + cofre de 32 B es lo que recomienda NIST SP 800-38D
//! para hasta 2^32 mensajes con la misma clave (suficiente para una
//! sesión de onion routing).
//!
//! Patrón de uso en KEM-DEM (ML-KEM → AES-256-GCM):
//!
//! ```ignore
//! use common::crypto::{aead, pqc};
//!
//! // origen
//! let kem = pqc::kem_for(pqc::suite::ML_KEM_768)?;
//! let kp  = kem.keygen()?;                        // se publica kp.public
//! let enc = kem.encap(&peer_public)?;             // viaja enc.ciphertext
//! let msg = aead::seal(&enc.shared_secret, b"hola", &[])?;
//!
//! // destino
//! let ss  = kem.decap(&kp.secret, &enc.ciphertext)?;
//! let pt  = aead::open(&ss, &msg, &[])?;
//! assert_eq!(&pt, b"hola");
//! ```

use aes_gcm::{
    aead::{Aead, AeadCore, AeadInPlace, KeyInit, OsRng, Payload},
    Aes256Gcm, Key, Nonce,
};
use thiserror::Error;

/// Tamaño de clave AES-256 en bytes.
pub const KEY_LEN: usize = 32;
/// Tamaño del nonce GCM en bytes (96 bits, recomendado por NIST).
pub const NONCE_LEN: usize = 12;
/// Tamaño del tag GCM en bytes (128 bits, full tag — el por defecto).
pub const TAG_LEN: usize = 16;

#[derive(Debug, Error)]
pub enum AeadError {
    #[error("bad key length: expected {KEY_LEN}, got {0}")]
    BadKeyLen(usize),
    #[error("bad nonce length: expected {NONCE_LEN}, got {0}")]
    BadNonceLen(usize),
    #[error("aead encrypt failed")]
    Encrypt,
    #[error("aead decrypt failed (auth tag mismatch or tampered ciphertext)")]
    Decrypt,
}

/// Mensaje sellado: `nonce || ciphertext` separados para que el caller
/// los serialice como quiera. `ciphertext` ya incluye el tag GCM
/// adjunto al final.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedMessage {
    pub nonce: [u8; NONCE_LEN],
    pub ciphertext: Vec<u8>,
}

impl SealedMessage {
    /// Serialización compacta: `nonce(12) || ciphertext+tag`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(NONCE_LEN + self.ciphertext.len());
        out.extend_from_slice(&self.nonce);
        out.extend_from_slice(&self.ciphertext);
        out
    }

    /// Inversa de [`to_bytes`]. Requiere al menos `NONCE_LEN + TAG_LEN`
    /// bytes (nonce + tag mínimo, payload puede ser vacío).
    pub fn from_bytes(buf: &[u8]) -> Result<Self, AeadError> {
        if buf.len() < NONCE_LEN + TAG_LEN {
            return Err(AeadError::Decrypt);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&buf[..NONCE_LEN]);
        Ok(Self {
            nonce,
            ciphertext: buf[NONCE_LEN..].to_vec(),
        })
    }
}

/// Cifra y autentica `plaintext` con `key` (32 B). `aad` opcional:
/// bytes adicionales que se autentican pero no se cifran (cabeceras,
/// metadatos de routing, etc.). Pasar `&[]` si no hay AAD.
pub fn seal(key: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<SealedMessage, AeadError> {
    if key.len() != KEY_LEN {
        return Err(AeadError::BadKeyLen(key.len()));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng); // 12 B random
    let ct = cipher
        .encrypt(
            &nonce,
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| AeadError::Encrypt)?;
    let mut nonce_out = [0u8; NONCE_LEN];
    nonce_out.copy_from_slice(nonce.as_slice());
    Ok(SealedMessage {
        nonce: nonce_out,
        ciphertext: ct,
    })
}

/// Sella con nonce **explícito** y tag **separado** del ciphertext.
///
/// Existe por una razón muy concreta: en la cebolla del ORR ni el nonce ni el
/// tag son secretos, y meterlos dentro del payload cuesta **claves QKD**. El
/// OTP del enlace trocea en bloques de `key_size_bits / 8` —32 bytes por
/// defecto— y consume una clave por bloque, así que los 28 bytes de
/// `nonce ‖ tag` convierten un mensaje de 32 bytes en dos bloques: el doble de
/// material QKD por salto. Medido en CESGA el 2026-08-28: −51 % de rendimiento,
/// con `wenc_to` y `misses` disparados en el keystore.
///
/// Sacándolos del payload, el ciphertext mide exactamente lo que el plaintext y
/// el troceado no cambia.
///
/// **El caller responde de que el nonce no se repita con la misma clave.** El
/// uso seguro es derivarlo de lo mismo que deriva la clave: si cada clave se usa
/// con un único nonce, la reutilización es imposible por construcción.
pub fn seal_detached(
    key: &[u8],
    nonce: &[u8; NONCE_LEN],
    plaintext: &[u8],
    aad: &[u8],
) -> Result<(Vec<u8>, [u8; TAG_LEN]), AeadError> {
    if key.len() != KEY_LEN {
        return Err(AeadError::BadKeyLen(key.len()));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut buf = plaintext.to_vec();
    let tag = cipher
        .encrypt_in_place_detached(Nonce::from_slice(nonce), aad, &mut buf)
        .map_err(|_| AeadError::Encrypt)?;
    let mut tag_out = [0u8; TAG_LEN];
    tag_out.copy_from_slice(tag.as_slice());
    Ok((buf, tag_out))
}

/// Inversa de [`seal_detached`].
pub fn open_detached(
    key: &[u8],
    nonce: &[u8; NONCE_LEN],
    ciphertext: &[u8],
    tag: &[u8; TAG_LEN],
    aad: &[u8],
) -> Result<Vec<u8>, AeadError> {
    if key.len() != KEY_LEN {
        return Err(AeadError::BadKeyLen(key.len()));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let mut buf = ciphertext.to_vec();
    cipher
        .decrypt_in_place_detached(
            Nonce::from_slice(nonce),
            aad,
            &mut buf,
            aes_gcm::Tag::from_slice(tag),
        )
        .map_err(|_| AeadError::Decrypt)?;
    Ok(buf)
}

/// Descifra y verifica. Devuelve error si la clave es incorrecta, el
/// nonce no coincide, el `aad` no coincide o el ciphertext fue
/// manipulado (tag GCM no valida).
pub fn open(key: &[u8], msg: &SealedMessage, aad: &[u8]) -> Result<Vec<u8>, AeadError> {
    if key.len() != KEY_LEN {
        return Err(AeadError::BadKeyLen(key.len()));
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key));
    let nonce = Nonce::from_slice(&msg.nonce);
    cipher
        .decrypt(
            nonce,
            Payload {
                msg: &msg.ciphertext,
                aad,
            },
        )
        .map_err(|_| AeadError::Decrypt)
}

#[cfg(test)]
mod tests_detached {
    use super::*;

    #[test]
    fn detached_round_trip_keeps_the_ciphertext_length() {
        // LA propiedad que importa: el ciphertext mide lo MISMO que el
        // plaintext. Es lo que evita cruzar el bloque del OTP y gastar una
        // segunda clave QKD por salto.
        let key = [7u8; KEY_LEN];
        let nonce = [3u8; NONCE_LEN];
        let pt = vec![0xABu8; 32];
        let (ct, tag) = seal_detached(&key, &nonce, &pt, b"aad").unwrap();
        assert_eq!(ct.len(), pt.len(), "el detached no debe alargar el payload");
        assert_eq!(tag.len(), TAG_LEN);
        assert_eq!(open_detached(&key, &nonce, &ct, &tag, b"aad").unwrap(), pt);
    }

    #[test]
    fn detached_catches_tampering_and_wrong_aad() {
        let key = [7u8; KEY_LEN];
        let nonce = [3u8; NONCE_LEN];
        let (mut ct, tag) = seal_detached(&key, &nonce, b"hola", b"aad").unwrap();
        assert!(open_detached(&key, &nonce, &ct, &tag, b"otro").is_err(), "aad");
        ct[0] ^= 0xFF;
        assert!(open_detached(&key, &nonce, &ct, &tag, b"aad").is_err(), "ct");
        let (ct2, mut tag2) = seal_detached(&key, &nonce, b"hola", b"aad").unwrap();
        tag2[0] ^= 0xFF;
        assert!(open_detached(&key, &nonce, &ct2, &tag2, b"aad").is_err(), "tag");
        let mut n2 = nonce;
        n2[0] ^= 0xFF;
        assert!(open_detached(&key, &n2, &ct2, &tag, b"aad").is_err(), "nonce");
    }

    #[test]
    fn detached_and_attached_agree() {
        // Mismo cifrado por debajo: `seal` = nonce ‖ detached(ct) ‖ tag.
        let key = [9u8; KEY_LEN];
        let sealed = seal(&key, b"mensaje", b"aad").unwrap();
        let (ct, tag) = seal_detached(&key, &sealed.nonce, b"mensaje", b"aad").unwrap();
        assert_eq!(&sealed.ciphertext[..ct.len()], &ct[..]);
        assert_eq!(&sealed.ciphertext[ct.len()..], &tag[..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::pqc;

    #[test]
    fn seal_open_round_trip() {
        let key = [0x42u8; KEY_LEN];
        let sealed = seal(&key, b"hello world", b"hdr").unwrap();
        let pt = open(&key, &sealed, b"hdr").unwrap();
        assert_eq!(pt, b"hello world");
    }

    #[test]
    fn wrong_aad_fails() {
        let key = [0x42u8; KEY_LEN];
        let sealed = seal(&key, b"hello", b"hdr").unwrap();
        assert!(matches!(
            open(&key, &sealed, b"NOPE"),
            Err(AeadError::Decrypt)
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = [0x01u8; KEY_LEN];
        let key2 = [0x02u8; KEY_LEN];
        let sealed = seal(&key1, b"hi", &[]).unwrap();
        assert!(matches!(open(&key2, &sealed, &[]), Err(AeadError::Decrypt)));
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let key = [0u8; KEY_LEN];
        let mut sealed = seal(&key, b"hello", &[]).unwrap();
        sealed.ciphertext[0] ^= 0x01;
        assert!(matches!(open(&key, &sealed, &[]), Err(AeadError::Decrypt)));
    }

    #[test]
    fn serialize_round_trip() {
        let key = [0u8; KEY_LEN];
        let sealed = seal(&key, b"data", &[]).unwrap();
        let bytes = sealed.to_bytes();
        let restored = SealedMessage::from_bytes(&bytes).unwrap();
        assert_eq!(sealed, restored);
        let pt = open(&key, &restored, &[]).unwrap();
        assert_eq!(pt, b"data");
    }

    #[test]
    fn bad_key_length_rejected() {
        let err = seal(&[0u8; 16], b"x", &[]).unwrap_err();
        assert!(matches!(err, AeadError::BadKeyLen(16)));
    }

    /// Patrón KEM-DEM completo: ML-KEM-768 + AES-256-GCM. Esto es lo
    /// que se usa en el handshake del onion routing.
    #[test]
    fn kem_dem_round_trip() {
        let kem = pqc::kem_for(pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        let enc = kem.encap(&kp.public).unwrap();
        // Origen sella con shared_secret salido del encap.
        let sealed = seal(&enc.shared_secret, b"onion payload", b"orr-hdr").unwrap();

        // Destino: recupera ss con decap usando su sk.
        let ss = kem.decap(&kp.secret, &enc.ciphertext).unwrap();
        let pt = open(&ss, &sealed, b"orr-hdr").unwrap();
        assert_eq!(pt, b"onion payload");
    }
}
