//! Onion routing PQC con XOR-cipher (OTP-style).
//!
//! Esquema por capa (un hop):
//!
//! 1. Para cifrar un `plaintext` de `N` bytes al ORR receptor cuya
//!    public key ML-KEM es `pk`, hacemos `K = ceil(N/32)` llamadas
//!    independientes a `kem.encap(pk)`. Cada una produce un
//!    `(ct_i, ss_i)` con `ss_i` de 32 B (FIPS 203). Los `ss_i` se
//!    concatenan, truncando a `N`, formando una "OTP key" `K_otp`.
//! 2. El ciphertext de la capa es `xor_ct = plaintext ⊕ K_otp`.
//! 3. La capa se serializa como `OnionFrame { kem_cts: [ct_1..ct_K], xor_ct }`.
//!
//! En el receptor: decapsula cada `ct_i` con su sk (mismo `K_otp`) y
//! reconstruye el plaintext con XOR.
//!
//! **Importante** (decidido por el usuario): no hay MAC. XOR puro es
//! maleable — un atacante con acceso al `xor_ct` puede flippear bits
//! en el plaintext sin que el receptor lo detecte. Es coherente con el
//! modelo OTP "literal" que usa el QKC con material QKD, asumiendo
//! que la confidencialidad y la integridad del transporte la pone el
//! QKD-OTP del enlace.
//!
//! **Coste de red** por capa con ML-KEM-768: cada `ct_i` ocupa 1088 B,
//! así que cifrar `N = 1024 B` envía 32 × 1088 ≈ 35 KB de ciphertexts
//! KEM por hop. Es voluminoso; si en el futuro el coste pesa, conviene
//! cambiar a un único encap + KDF expansión (`HKDF-SHA256`).
//!
//! Construcción del onion completo (`build_onion`):
//!
//! ```text
//! payload    →  Deliver { payload }          ⊕  K_otp(pk_dst)      = inner_N (bytes)
//! inner_N    →  Forward { next=dst, inner }  ⊕  K_otp(pk_hop_N-1)  = inner_N-1
//!  ...
//! inner_2    →  Forward { next=hop_2, inner }⊕  K_otp(pk_hop_1)    = outer
//! ```
//!
//! La capa más externa se mete en el `payload` del `FRAME_LOCAL_SEND`
//! dirigido al QKC del primer hop.

use common::crypto::pqc::Kem;
use serde::{Deserialize, Serialize};

use crate::error::{OrrError, Result};

/// Una capa onion ya cifrada para un único hop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OnionFrame {
    /// `ceil(N/32)` ciphertexts ML-KEM independientes. Cada uno se
    /// decapsula al mismo `pk` del receptor y produce 32 B de shared
    /// secret. Concatenados forman la "OTP key" de `N` bytes.
    pub kem_cts: Vec<Vec<u8>>,
    /// `plaintext ⊕ key_otp` (longitud = `N`, longitud del plaintext
    /// original de esta capa).
    pub xor_ct: Vec<u8>,
}

impl OnionFrame {
    /// Serializa la frame con msgpack-named (mismo formato que las
    /// cabeceras ORR para mantener un único encoder).
    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self).map_err(|e| OrrError::Relay(format!("onion encode: {e}")))
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(buf).map_err(|e| OrrError::Relay(format!("onion decode: {e}")))
    }
}

/// Resultado de pelar una capa: o reenviar al siguiente ORR, o
/// entregar el payload final a la aplicación.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum InnerLayer {
    /// Hop intermedio. `inner` son los bytes (ya msgpack-encodificados)
    /// del siguiente `OnionFrame` — el receptor los pone tal cual en
    /// el `payload` del nuevo `FRAME_LOCAL_SEND` al QKC del siguiente
    /// hop.
    Forward {
        next_orr_id: String,
        next_qkc_id: u32,
        inner:       Vec<u8>,
    },
    /// Hop final: el payload original a entregar al DKMS local.
    Deliver { payload: Vec<u8> },
}

impl InnerLayer {
    fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self).map_err(|e| OrrError::Relay(format!("inner encode: {e}")))
    }

    fn decode(buf: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(buf).map_err(|e| OrrError::Relay(format!("inner decode: {e}")))
    }
}

/// Hop en el path del onion.
#[derive(Debug, Clone)]
pub struct PathHop {
    pub orr_id:     String,
    pub qkc_id:     u32,
    pub public_key: Vec<u8>,
}

/// Cifra `plaintext` para un único hop. Hace tantos encaps como hagan
/// falta (ceil(N/32)) y concatena los shared secrets como key OTP.
pub fn wrap_layer(kem: &dyn Kem, peer_pk: &[u8], plaintext: &[u8]) -> Result<OnionFrame> {
    let n = plaintext.len();
    let chunks = n.div_ceil(32).max(1); // siempre 1 encap mínimo para mantener "1 layer = ≥1 ct"
    let mut kem_cts = Vec::with_capacity(chunks);
    let mut key_otp = Vec::with_capacity(chunks * 32);
    for _ in 0..chunks {
        let encap = kem.encap(peer_pk)?;
        kem_cts.push(encap.ciphertext);
        key_otp.extend_from_slice(&encap.shared_secret);
    }
    key_otp.truncate(n);

    let mut xor_ct = Vec::with_capacity(n);
    for (p, k) in plaintext.iter().zip(key_otp.iter()) {
        xor_ct.push(p ^ k);
    }
    Ok(OnionFrame { kem_cts, xor_ct })
}

/// Descifra una capa: decapsula cada `kem_ct` con `my_sk`,
/// concatena los shared secrets, XOR con `xor_ct`.
pub fn unwrap_layer(kem: &dyn Kem, my_sk: &[u8], frame: &OnionFrame) -> Result<Vec<u8>> {
    let n = frame.xor_ct.len();
    let expected_chunks = n.div_ceil(32).max(1);
    if frame.kem_cts.len() != expected_chunks {
        return Err(OrrError::Relay(format!(
            "onion: kem_cts count {} != expected {} (payload len {})",
            frame.kem_cts.len(),
            expected_chunks,
            n
        )));
    }
    let mut key_otp = Vec::with_capacity(expected_chunks * 32);
    for ct in &frame.kem_cts {
        let ss = kem.decap(my_sk, ct)?;
        key_otp.extend_from_slice(&ss);
    }
    key_otp.truncate(n);
    if key_otp.len() < n {
        return Err(OrrError::Relay(format!(
            "onion: not enough key material ({} < {})",
            key_otp.len(),
            n
        )));
    }
    let mut pt = Vec::with_capacity(n);
    for (c, k) in frame.xor_ct.iter().zip(key_otp.iter()) {
        pt.push(c ^ k);
    }
    Ok(pt)
}

/// Construye un onion completo dado el `path` (lista de hops ordenados,
/// **excluyendo** al ORR origen — el primer elemento es el primer hop
/// al que se enviará, el último es el destino final que entrega al
/// DKMS).
///
/// Devuelve los bytes serializados de la capa más externa, ya listos
/// para meterlos en el `payload` del `FRAME_LOCAL_SEND` dirigido al
/// QKC del primer hop.
pub fn build_onion(kem: &dyn Kem, path: &[PathHop], payload: Vec<u8>) -> Result<Vec<u8>> {
    if path.is_empty() {
        return Err(OrrError::Relay("onion: empty path".into()));
    }

    // Innermost: Deliver { payload } cifrado para el destino final.
    let dst = path.last().unwrap();
    let inner_pt = InnerLayer::Deliver { payload }.encode()?;
    let frame = wrap_layer(kem, &dst.public_key, &inner_pt)?;
    let mut current_bytes = frame.encode()?;

    // Reenvolver hacia fuera: cada hop intermedio recibe un
    // InnerLayer::Forward que contiene el bundle siguiente.
    for i in (0..path.len() - 1).rev() {
        let hop = &path[i];
        let next = &path[i + 1];
        let layer = InnerLayer::Forward {
            next_orr_id: next.orr_id.clone(),
            next_qkc_id: next.qkc_id,
            inner:       current_bytes,
        };
        let layer_pt = layer.encode()?;
        let frame = wrap_layer(kem, &hop.public_key, &layer_pt)?;
        current_bytes = frame.encode()?;
    }
    Ok(current_bytes)
}

/// Pela una capa ya parseada y devuelve la decisión.
pub fn peel(kem: &dyn Kem, my_sk: &[u8], frame_bytes: &[u8]) -> Result<InnerLayer> {
    let frame = OnionFrame::decode(frame_bytes)?;
    let pt = unwrap_layer(kem, my_sk, &frame)?;
    InnerLayer::decode(&pt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::crypto::pqc::{kem_for, suite};

    fn make_hop(kem: &dyn Kem, id: &str, qkc: u32) -> (PathHop, Vec<u8>) {
        let kp = kem.keygen().unwrap();
        (
            PathHop {
                orr_id:     id.into(),
                qkc_id:     qkc,
                public_key: kp.public.clone(),
            },
            kp.secret,
        )
    }

    #[test]
    fn single_hop_round_trip() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        let (hop, sk) = make_hop(&*kem, "ORR_DST", 9);
        let payload = b"hola onion mundo".to_vec();
        let bundle = build_onion(&*kem, &[hop], payload.clone()).unwrap();

        match peel(&*kem, &sk, &bundle).unwrap() {
            InnerLayer::Deliver { payload: out } => assert_eq!(out, payload),
            _ => panic!("expected Deliver"),
        }
    }

    #[test]
    fn three_hop_round_trip() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        let (b, sk_b) = make_hop(&*kem, "ORR_B", 2);
        let (c, sk_c) = make_hop(&*kem, "ORR_C", 3);
        let (d, sk_d) = make_hop(&*kem, "ORR_D", 4);
        let payload = b"viajando por la cebolla".to_vec();
        let path = vec![b.clone(), c.clone(), d.clone()];

        // origen → B
        let outer = build_onion(&*kem, &path, payload.clone()).unwrap();

        // B pela: debería decirme que reenvíe a C.
        let layer_b = peel(&*kem, &sk_b, &outer).unwrap();
        let inner_b = match layer_b {
            InnerLayer::Forward { next_orr_id, next_qkc_id, inner } => {
                assert_eq!(next_orr_id, "ORR_C");
                assert_eq!(next_qkc_id, 3);
                inner
            }
            _ => panic!("expected Forward at B"),
        };

        // C pela: reenviar a D.
        let layer_c = peel(&*kem, &sk_c, &inner_b).unwrap();
        let inner_c = match layer_c {
            InnerLayer::Forward { next_orr_id, next_qkc_id, inner } => {
                assert_eq!(next_orr_id, "ORR_D");
                assert_eq!(next_qkc_id, 4);
                inner
            }
            _ => panic!("expected Forward at C"),
        };

        // D pela: Deliver con el payload original.
        match peel(&*kem, &sk_d, &inner_c).unwrap() {
            InnerLayer::Deliver { payload: out } => assert_eq!(out, payload),
            _ => panic!("expected Deliver at D"),
        }
    }

    #[test]
    fn empty_path_fails() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        assert!(build_onion(&*kem, &[], vec![1, 2, 3]).is_err());
    }

    #[test]
    fn wrong_sk_fails_to_decap() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        let (hop, _correct_sk) = make_hop(&*kem, "ORR_DST", 9);
        let wrong = kem.keygen().unwrap();
        let bundle = build_onion(&*kem, &[hop], b"x".to_vec()).unwrap();
        // Con sk equivocada el ML-KEM hace implicit rejection: devuelve
        // un ss derivado del Z interno (no panic). Lo que sí pasa es
        // que el plaintext recuperado no es el original; el siguiente
        // paso (InnerLayer::decode) probablemente falle al deserializar,
        // pero el contrato aquí sólo garantiza que NO devuelve el
        // mensaje original.
        let result = peel(&*kem, &wrong.secret, &bundle);
        match result {
            Err(_) => {} // decode failure, acceptable
            Ok(InnerLayer::Deliver { payload }) => assert_ne!(payload, b"x".to_vec()),
            Ok(_) => {} // structural mismatch, acceptable
        }
    }

    #[test]
    fn key_otp_chunks_match_payload_size() {
        // Verifica que el número de encaps escala con N.
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        let (hop, sk) = make_hop(&*kem, "ORR_DST", 9);
        for n in [1usize, 31, 32, 33, 100, 1024] {
            let pt = vec![0xAB; n];
            let frame = wrap_layer(&*kem, &hop.public_key, &pt).unwrap();
            assert_eq!(frame.xor_ct.len(), n);
            assert_eq!(frame.kem_cts.len(), n.div_ceil(32).max(1));
            let out = unwrap_layer(&*kem, &sk, &frame).unwrap();
            assert_eq!(out, pt);
        }
    }
}
