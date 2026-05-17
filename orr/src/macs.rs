//! HMAC helpers para el sub-protocolo de rotación de master_secret
//! (audit H-3 / Option B).
//!
//! Estos MACs autentican las RPCs `RequestEphemeralKey` y
//! `EstablishEphemeralSecret` del servicio `OrrControl`. La clave
//! HMAC es el `bootstrap_secret` derivado del ML-KEM encap inicial
//! contra la long-term pubkey del peer; **jamás se usa como
//! keystream**, sólo como input a HMAC-SHA256.
//!
//! ## Etiquetas de dominio
//!
//! Cada MAC lleva una etiqueta de prefijo en cleartext para evitar
//! que un mensaje válido para una fase del protocolo se reutilice en
//! otra (cross-protocol attack):
//!
//! | Etiqueta | Mensaje                      | Quién genera | Quién verifica |
//! |----------|------------------------------|--------------|----------------|
//! | `"REQ"`  | `RequestEphemeralKeyRequest` | initiator    | responder      |
//! | `"RESP"` | `RequestEphemeralKeyResponse`| responder    | initiator      |
//! | `"FIN"`  | `EstablishEphemeralSecretRequest` | initiator | responder      |
//!
//! ## Layout del input HMAC
//!
//! Cada campo variable (orr_id, pubkey, ciphertext) va length-prefixed
//! con `u16` big-endian para evitar ambigüedad de boundary entre
//! campos concatenados:
//!
//! ```text
//!   TAG (3-4 B) || epoch_id_be (4 B) ||
//!   lp16(from) || lp16(peer)
//!   [|| lp16(ephemeral_pubkey) o lp16(ciphertext)]
//!
//!   lp16(x) := u16_be(x.len()) || x
//! ```
//!
//! Sin `lp16`, los pares `(from="A", peer="BB")` y `(from="AB",
//! peer="B")` producen el mismo flujo de bytes y la misma MAC — una
//! confusión semántica sutil pero suficiente para un atacante que
//! controle nombres lógicos. Los IDs aquí no son adversariales (los
//! pone el operador), pero el coste de length-prefix es 2 B por
//! campo y elimina el footgun.
//!
//! ## Verificación
//!
//! Toda verificación pasa por `Hmac<Sha256>::verify_slice(tag)`
//! (constant-time interno). Nunca comparar con `==` o slice equality:
//! revelaría timing side channels.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Etiqueta dominio para `RequestEphemeralKeyRequest`.
pub const TAG_REQ: &[u8] = b"REQ";
/// Etiqueta dominio para `RequestEphemeralKeyResponse`.
pub const TAG_RESP: &[u8] = b"RESP";
/// Etiqueta dominio para `EstablishEphemeralSecretRequest`.
pub const TAG_FIN: &[u8] = b"FIN";

/// Error de verificación de MAC. El cuerpo no contiene detalles del
/// fallo para no facilitar enumeración (`MacError::Invalid` siempre).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MacError {
    #[error("invalid MAC")]
    Invalid,
}

/// `lp16(x)` = `u16_be(x.len()) || x`. Panica si `x.len() > u16::MAX`
/// (no aplica a orr_ids cortos ni a ML-KEM ciphertexts/pubkeys, todos
/// ≤ 1568 B para ML-KEM-1024).
fn write_lp16(mac: &mut HmacSha256, x: &[u8]) {
    debug_assert!(
        x.len() <= u16::MAX as usize,
        "lp16 overflow: {} bytes",
        x.len()
    );
    mac.update(&(x.len() as u16).to_be_bytes());
    mac.update(x);
}

fn fresh_mac(bootstrap: &[u8; 32]) -> HmacSha256 {
    // `Hmac::new_from_slice` no falla para claves de cualquier
    // longitud — el unwrap aquí es total (32 B siempre cabe).
    HmacSha256::new_from_slice(bootstrap).expect("HMAC accepts any key length")
}

// ─── Generación (initiator/responder) ─────────────────────────────────

/// MAC para `RequestEphemeralKeyRequest`. Lo construye el initiator.
pub fn mac_req(bootstrap: &[u8; 32], epoch_id: u32, from: &str, peer: &str) -> Vec<u8> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_REQ);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// MAC para `RequestEphemeralKeyResponse`. Lo construye el responder
/// tras generar `(epk, esk)` ML-KEM.
pub fn mac_resp(
    bootstrap: &[u8; 32],
    epoch_id: u32,
    from: &str,
    peer: &str,
    ephemeral_pubkey: &[u8],
) -> Vec<u8> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_RESP);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    write_lp16(&mut mac, ephemeral_pubkey);
    mac.finalize().into_bytes().to_vec()
}

/// MAC para `EstablishEphemeralSecretRequest`. Lo construye el
/// initiator tras `encap(ephemeral_pubkey)`.
pub fn mac_fin(
    bootstrap: &[u8; 32],
    epoch_id: u32,
    from: &str,
    peer: &str,
    ciphertext: &[u8],
) -> Vec<u8> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_FIN);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    write_lp16(&mut mac, ciphertext);
    mac.finalize().into_bytes().to_vec()
}

// ─── Verificación constant-time ───────────────────────────────────────

/// Verifica el MAC de `RequestEphemeralKeyRequest`. Lo llama el
/// responder al recibir la RPC. Constant-time vía
/// `Hmac::verify_slice`.
pub fn verify_mac_req(
    bootstrap: &[u8; 32],
    epoch_id: u32,
    from: &str,
    peer: &str,
    tag: &[u8],
) -> Result<(), MacError> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_REQ);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    mac.verify_slice(tag).map_err(|_| MacError::Invalid)
}

/// Verifica el MAC de `RequestEphemeralKeyResponse`. Lo llama el
/// initiator al recibir la pubkey efímera.
pub fn verify_mac_resp(
    bootstrap: &[u8; 32],
    epoch_id: u32,
    from: &str,
    peer: &str,
    ephemeral_pubkey: &[u8],
    tag: &[u8],
) -> Result<(), MacError> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_RESP);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    write_lp16(&mut mac, ephemeral_pubkey);
    mac.verify_slice(tag).map_err(|_| MacError::Invalid)
}

/// Verifica el MAC de `EstablishEphemeralSecretRequest`. Lo llama el
/// responder al recibir el ciphertext.
pub fn verify_mac_fin(
    bootstrap: &[u8; 32],
    epoch_id: u32,
    from: &str,
    peer: &str,
    ciphertext: &[u8],
    tag: &[u8],
) -> Result<(), MacError> {
    let mut mac = fresh_mac(bootstrap);
    mac.update(TAG_FIN);
    mac.update(&epoch_id.to_be_bytes());
    write_lp16(&mut mac, from.as_bytes());
    write_lp16(&mut mac, peer.as_bytes());
    write_lp16(&mut mac, ciphertext);
    mac.verify_slice(tag).map_err(|_| MacError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOOTSTRAP: [u8; 32] = [0x42; 32];

    #[test]
    fn req_round_trip() {
        let tag = mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b");
        assert!(verify_mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b", &tag).is_ok());
        assert_eq!(tag.len(), 32); // HMAC-SHA256 output
    }

    #[test]
    fn resp_round_trip() {
        let epk = vec![0xAA; 1184]; // ML-KEM-768 pubkey size
        let tag = mac_resp(&BOOTSTRAP, 7, "orr_a", "orr_b", &epk);
        assert!(verify_mac_resp(&BOOTSTRAP, 7, "orr_a", "orr_b", &epk, &tag).is_ok());
    }

    #[test]
    fn fin_round_trip() {
        let ct = vec![0xBB; 1088]; // ML-KEM-768 ciphertext size
        let tag = mac_fin(&BOOTSTRAP, 7, "orr_a", "orr_b", &ct);
        assert!(verify_mac_fin(&BOOTSTRAP, 7, "orr_a", "orr_b", &ct, &tag).is_ok());
    }

    #[test]
    fn req_rejects_wrong_bootstrap() {
        let tag = mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b");
        let other = [0x55; 32];
        assert_eq!(
            verify_mac_req(&other, 7, "orr_a", "orr_b", &tag),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn req_rejects_wrong_epoch() {
        let tag = mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b");
        assert_eq!(
            verify_mac_req(&BOOTSTRAP, 8, "orr_a", "orr_b", &tag),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn req_rejects_swapped_peers() {
        // El MAC depende del orden (from, peer): un MAC para (a, b) NO
        // pasa como MAC para (b, a). Importante porque las dos
        // direcciones del par usan el mismo bootstrap_secret.
        let tag = mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b");
        assert_eq!(
            verify_mac_req(&BOOTSTRAP, 7, "orr_b", "orr_a", &tag),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn tag_mismatch_between_phases() {
        // Mismo bootstrap_secret, mismo epoch_id, mismo (from, peer),
        // pero el MAC de REQ no pasa como MAC de FIN: la etiqueta
        // dominio los separa.
        let tag_req = mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b");
        let ct = vec![0xCC; 16];
        assert_eq!(
            verify_mac_fin(&BOOTSTRAP, 7, "orr_a", "orr_b", &ct, &tag_req),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn lp16_disambiguates_concatenation() {
        // Sin lp16, ("a", "bc") y ("ab", "c") producirían el mismo
        // input y mismo MAC. Con lp16, NO.
        let t1 = mac_req(&BOOTSTRAP, 1, "a", "bc");
        let t2 = mac_req(&BOOTSTRAP, 1, "ab", "c");
        assert_ne!(t1, t2);
    }

    #[test]
    fn resp_rejects_modified_pubkey() {
        let mut epk = vec![0xAA; 32];
        let tag = mac_resp(&BOOTSTRAP, 5, "orr_a", "orr_b", &epk);
        epk[0] ^= 0xFF;
        assert_eq!(
            verify_mac_resp(&BOOTSTRAP, 5, "orr_a", "orr_b", &epk, &tag),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn fin_rejects_modified_ciphertext() {
        let mut ct = vec![0xBB; 32];
        let tag = mac_fin(&BOOTSTRAP, 5, "orr_a", "orr_b", &ct);
        let last = ct.len() - 1;
        ct[last] ^= 0x01;
        assert_eq!(
            verify_mac_fin(&BOOTSTRAP, 5, "orr_a", "orr_b", &ct, &tag),
            Err(MacError::Invalid),
        );
    }

    #[test]
    fn req_verify_constant_time_path() {
        // No es un test de timing real (eso requeriría hardware
        // específico), pero verifica que un MAC de longitud incorrecta
        // (no-32 B) es rechazado limpiamente sin panic.
        let too_short = vec![0u8; 16];
        assert_eq!(
            verify_mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b", &too_short),
            Err(MacError::Invalid),
        );
        let too_long = vec![0u8; 64];
        assert_eq!(
            verify_mac_req(&BOOTSTRAP, 7, "orr_a", "orr_b", &too_long),
            Err(MacError::Invalid),
        );
    }
}
