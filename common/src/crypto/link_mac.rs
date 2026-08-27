//! HMAC-SHA256 para autenticar el handshake PQC del enlace QKC↔QKC
//! (docs/SECURITY.md §Fase 5).
//!
//! El payload del enlace ya va cifrado OTP con claves QKD/PQC, pero el
//! **handshake ML-KEM no está firmado**: un MITM activo puede sentarse en
//! medio del INIT/RESP y quedarse con los dos secretos, y un `INIT` forjado
//! con otra pubkey para una época ya establecida hace que el responder
//! sobrescriba el secreto vivo. Este MAC ata cada mensaje del handshake a un
//! **secreto pre-compartido por enlace** (`link_psk`, configurado localmente
//! en cada QKC — el SDN no transporta secretos).
//!
//! HMAC es simétrico → sigue siendo seguro frente a un adversario cuántico.
//! ML-DSA (firma PQC) queda como upgrade documentado (frames 0x26/0x27), para
//! cuando el QKC tenga distribución de pubkeys de firma.
//!
//! ## Etiquetas de dominio y layout
//!
//! ```text
//!   TAG || epoch_be(4 B) || u32_be(sender_id) || u32_be(receiver_id)
//!       || lp16(blob) || lp16(suite_id) || key_size_bits_be(4 B)
//!   lp16(x) := u16_be(x.len()) || x
//! ```
//!
//! Atar `sender_id`/`receiver_id` fija la identidad (que en el wire es un u32
//! sin verificar); atar `suite_id`/`key_size_bits` cierra el mismatch
//! silencioso (ambos son config-only y no viajan en el payload). La etiqueta
//! separa INIT/RESP/NOTIFY (cross-protocol). Verificación constant-time vía
//! `Hmac::verify_slice`.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Etiqueta de dominio del `FRAME_PQC_KEM_INIT_AUTH`.
pub const TAG_INIT: &[u8] = b"QKCINIT";
/// Etiqueta de dominio del `FRAME_PQC_KEM_RESP_AUTH`.
pub const TAG_RESP: &[u8] = b"QKCRESP";
/// Etiqueta de dominio del `FRAME_KEY_IDS_NOTIFY_AUTH`.
pub const TAG_NOTIFY: &[u8] = b"QKCNOTIFY";

/// Longitud del tag HMAC-SHA256 que se anexa al payload del frame.
pub const TAG_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MacError {
    #[error("invalid link MAC")]
    Invalid,
}

fn write_lp16(mac: &mut HmacSha256, x: &[u8]) {
    debug_assert!(x.len() <= u16::MAX as usize, "lp16 overflow: {}", x.len());
    mac.update(&(x.len() as u16).to_be_bytes());
    mac.update(x);
}

fn fresh(psk: &[u8]) -> HmacSha256 {
    HmacSha256::new_from_slice(psk).expect("HMAC accepts any key length")
}

fn build(
    psk: &[u8],
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
) -> Vec<u8> {
    let mut mac = fresh(psk);
    mac.update(tag);
    mac.update(&epoch.to_be_bytes());
    mac.update(&sender_id.to_be_bytes());
    mac.update(&receiver_id.to_be_bytes());
    write_lp16(&mut mac, blob);
    write_lp16(&mut mac, suite_id.as_bytes());
    mac.update(&key_size_bits.to_be_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// Calcula el tag HMAC de un mensaje del handshake. `tag` es una de las
/// etiquetas `TAG_INIT`/`TAG_RESP`/`TAG_NOTIFY`. `blob` es el cuerpo del
/// mensaje (pubkey/ciphertext ML-KEM, o los key_ids del NOTIFY).
#[allow(clippy::too_many_arguments)]
pub fn tag(
    psk: &[u8],
    tag: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
) -> Vec<u8> {
    build(
        psk,
        tag,
        epoch,
        sender_id,
        receiver_id,
        blob,
        suite_id,
        key_size_bits,
    )
}

/// Verifica el tag HMAC de un mensaje del handshake (constant-time).
#[allow(clippy::too_many_arguments)]
pub fn verify(
    psk: &[u8],
    tag_kind: &[u8],
    epoch: u32,
    sender_id: u32,
    receiver_id: u32,
    blob: &[u8],
    suite_id: &str,
    key_size_bits: u32,
    mac: &[u8],
) -> Result<(), MacError> {
    let mut h = fresh(psk);
    h.update(tag_kind);
    h.update(&epoch.to_be_bytes());
    h.update(&sender_id.to_be_bytes());
    h.update(&receiver_id.to_be_bytes());
    write_lp16(&mut h, blob);
    write_lp16(&mut h, suite_id.as_bytes());
    h.update(&key_size_bits.to_be_bytes());
    h.verify_slice(mac).map_err(|_| MacError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: &[u8] = b"a-32-byte-or-any-length-psk-value";

    fn t(kind: &[u8], epoch: u32, s: u32, r: u32, blob: &[u8]) -> Vec<u8> {
        tag(PSK, kind, epoch, s, r, blob, "ml-kem-768", 1024)
    }
    fn v(kind: &[u8], epoch: u32, s: u32, r: u32, blob: &[u8], mac: &[u8]) -> Result<(), MacError> {
        verify(PSK, kind, epoch, s, r, blob, "ml-kem-768", 1024, mac)
    }

    #[test]
    fn round_trip_and_len() {
        let blob = vec![0xAA; 1184];
        let m = t(TAG_INIT, 7, 1, 2, &blob);
        assert_eq!(m.len(), TAG_LEN);
        assert!(v(TAG_INIT, 7, 1, 2, &blob, &m).is_ok());
    }

    #[test]
    fn rejects_wrong_psk_epoch_ids_blob() {
        let blob = vec![0xBB; 64];
        let m = t(TAG_INIT, 7, 1, 2, &blob);
        // psk distinto
        assert_eq!(
            verify(b"other", TAG_INIT, 7, 1, 2, &blob, "ml-kem-768", 1024, &m),
            Err(MacError::Invalid)
        );
        // epoch distinto
        assert_eq!(v(TAG_INIT, 8, 1, 2, &blob, &m), Err(MacError::Invalid));
        // sender/receiver intercambiados (mismo enlace, misma PSK: crítico)
        assert_eq!(v(TAG_INIT, 7, 2, 1, &blob, &m), Err(MacError::Invalid));
        // blob modificado
        let mut b2 = blob.clone();
        b2[0] ^= 0xFF;
        assert_eq!(v(TAG_INIT, 7, 1, 2, &b2, &m), Err(MacError::Invalid));
    }

    #[test]
    fn domain_tags_dont_cross() {
        let blob = vec![1, 2, 3];
        let m = t(TAG_INIT, 1, 1, 2, &blob);
        assert_eq!(v(TAG_RESP, 1, 1, 2, &blob, &m), Err(MacError::Invalid));
        assert_eq!(v(TAG_NOTIFY, 1, 1, 2, &blob, &m), Err(MacError::Invalid));
    }

    #[test]
    fn suite_and_key_size_are_bound() {
        let blob = vec![9; 8];
        let m = tag(PSK, TAG_INIT, 3, 1, 2, &blob, "ml-kem-768", 1024);
        // suite distinta
        assert_eq!(
            verify(PSK, TAG_INIT, 3, 1, 2, &blob, "ml-kem-512", 1024, &m),
            Err(MacError::Invalid)
        );
        // key_size distinto
        assert_eq!(
            verify(PSK, TAG_INIT, 3, 1, 2, &blob, "ml-kem-768", 256, &m),
            Err(MacError::Invalid)
        );
    }

    #[test]
    fn wrong_length_mac_rejected_cleanly() {
        let blob = vec![0; 4];
        assert_eq!(v(TAG_INIT, 1, 1, 2, &blob, &[0u8; 16]), Err(MacError::Invalid));
        assert_eq!(v(TAG_INIT, 1, 1, 2, &blob, &[0u8; 64]), Err(MacError::Invalid));
    }
}
