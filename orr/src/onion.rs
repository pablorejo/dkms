//! Onion routing v3 con master_secret pre-compartido + HKDF-XOR.
//!
//! Cada par de ORRs comparte un `master_secret` de 32 B (vía ML-KEM
//! encap al arrancar, RPC `EstablishSecret`). Por cada frame onion, el
//! origen elige un `key_id` UUID v4 y deriva:
//!
//! ```text
//! K = HKDF-SHA256(salt=b"orr.onion.v1",
//!                 ikm=master_secret,
//!                 info=key_id ‖ u32_be(len) ‖ u32_be(chunk_idx),
//!                 L=len)
//! ```
//!
//! `xor_ct = plaintext ⊕ K`. El receptor, que tiene el mismo
//! `master_secret`, vuelve a derivar K idéntica con `key_id` (que viaja
//! en cleartext en `header_orr_mp`) y hace XOR.
//!
//! ## Construcción multi-capa
//!
//! Para path `[X1, X2, ..., Xn]` (X_n = destino final):
//!
//! ```text
//! innermost = body ⊕ K_{O,Xn}(kid_n, len_body)
//! layer_{n-1} = msgpack(Inner{next=Xn, kid=kid_n, xor=innermost})
//!               ⊕ K_{O,X_{n-1}}(kid_{n-1}, ...)
//! ...
//! outermost  = msgpack(Inner{next=X2, kid=kid_2, xor=layer_2})
//!               ⊕ K_{O,X1}(kid_1, ...)
//! ```
//!
//! El header del wire frame externo lleva `from=O`, `to=Xn`,
//! `next_orr_id=X1`, `key_id=kid_1`, `max_hops=n-1`. El `payload` del
//! wire frame es `outermost`. El QKC lo OTP-cifra en cada enlace
//! QKC↔QKC del path.
//!
//! ## Peeling
//!
//! El ORR X1 recibe del QKC:
//!
//! 1. Lee header → `next_orr_id == self`, `key_id`, `max_hops`,
//!    `from`. Busca `master_secret` indexado por `from`.
//! 2. `K = HKDF(master_secret, key_id, len(payload))`.
//! 3. `inner = payload ⊕ K`.
//! 4. Si `max_hops > 0`: inner es `InnerLayer { next=X2, kid=kid_2,
//!    xor=layer_2 }`. Construye nuevo wire frame con header
//!    reescrito y reenvía al QKC del X2.
//! 5. Si `max_hops == 0`: inner es `body_dkms` directo. Entrega local.
//!
//! ## Tamaño (vs. esquema v2)
//!
//! `InnerLayer` msgpack-named overhead ≈ 45 B (next_orr_id ~22 B +
//! key_id 18 B + length prefix 3 B + struct overhead 4 B). Crecimiento
//! **lineal** en nº de hops:
//!
//! ```text
//! modo 0 (passthrough):  64 B
//! modo 1 (1 capa):       64 B (xor_ct directo)
//! modo 2 (2 capas):      64 + 45 = 109 B
//! modo -1 (3 capas):     64 + 2·45 ≈ 154 B
//! ```
//!
//! El esquema v2 (`Vec<kem_ct>` per layer) explotaba a ~4 MB en modo -1.

use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;

use crate::error::{OrrError, Result};

const HKDF_SALT: &[u8] = b"orr.onion.v1";
const HKDF_MAX_OUTPUT: usize = 255 * 32; // HKDF-SHA256 ceiling: 8160 B

/// Una capa interna ya cifrada, lista para meter en el `payload` del
/// wire frame de la capa siguiente (más externa).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InnerLayer {
    /// ORR destino de la siguiente capa (el peeler que mirará `key_id`
    /// para derivar K y descifrar `xor_ct`).
    pub next_orr_id: String,
    /// UUID v4 raw (16 B) de la K de la siguiente capa.
    pub key_id: [u8; 16],
    /// `K ⊕ inner_plaintext` de la siguiente capa.
    pub xor_ct: Vec<u8>,
}

impl InnerLayer {
    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self)
            .map_err(|e| OrrError::Relay(format!("inner encode: {e}")))
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(buf)
            .map_err(|e| OrrError::Relay(format!("inner decode: {e}")))
    }
}

/// Hop en el path del onion: orr_id + master_secret compartido (lo que
/// produjo el ML-KEM encap al arrancar). El caller (service.rs) lo
/// construye consultando `PeerRegistry`.
#[derive(Debug, Clone)]
pub struct PathHopSecret {
    pub orr_id:        String,
    pub master_secret: [u8; 32],
}

/// Output de `build_onion`: lo que el caller necesita para armar el
/// wire frame externo.
#[derive(Debug, Clone)]
pub struct OnionWire {
    /// Primer hop del path (= `header.next_orr_id` del wire frame).
    pub first_hop_orr: String,
    /// `key_id` UUID v4 de la capa más externa (= `header.key_id`).
    pub first_key_id:  [u8; 16],
    /// Hops onion restantes tras pelar la capa externa (= `header.max_hops`).
    /// Si el path tiene N hops, este valor es N-1.
    pub max_hops:      i32,
    /// El payload cifrado de la capa externa: `K ⊕ inner`. Va en el
    /// `payload` del wire frame.
    pub payload:       Vec<u8>,
}

/// Resultado de pelar una capa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peeled {
    /// Capa intermedia: el caller construye un nuevo wire frame con
    /// `header.next_orr_id=inner.next_orr_id, key_id=inner.key_id,
    /// max_hops=prev_max_hops-1, payload=inner.xor_ct`.
    Forward(InnerLayer),
    /// Capa terminal: bytes del body_dkms directos para entregar.
    Deliver(Vec<u8>),
}

/// Deriva la K per-frame con HKDF-SHA256, soportando body_len arbitrario
/// vía chunking (cada chunk usa un `chunk_idx` distinto en el `info`
/// para producir keystream independiente).
pub fn derive_key(master_secret: &[u8; 32], key_id: &[u8; 16], body_len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), master_secret);
    let mut okm = vec![0u8; body_len];
    let mut info = [0u8; 16 + 4 + 4]; // kid ‖ u32_be(len) ‖ u32_be(chunk_idx)
    info[..16].copy_from_slice(key_id);
    info[16..20].copy_from_slice(&(body_len as u32).to_be_bytes());

    let mut off = 0usize;
    let mut chunk_idx: u32 = 0;
    while off < body_len {
        let take = (body_len - off).min(HKDF_MAX_OUTPUT);
        info[20..24].copy_from_slice(&chunk_idx.to_be_bytes());
        hk.expand(&info, &mut okm[off..off + take])
            .expect("HKDF-SHA256 expand within 8160 B chunk");
        off += take;
        chunk_idx = chunk_idx.checked_add(1).expect("body_len overflows chunk_idx u32");
    }
    okm
}

/// XOR del plaintext con la K derivada. XOR es simétrico → cifrar y
/// descifrar usan esta misma función.
fn xor_with_derived_key(
    master_secret: &[u8; 32],
    key_id: &[u8; 16],
    data: &[u8],
) -> Vec<u8> {
    let k = derive_key(master_secret, key_id, data.len());
    let mut out = Vec::with_capacity(data.len());
    for (b, kb) in data.iter().zip(k.iter()) {
        out.push(b ^ kb);
    }
    out
}

/// Construye un onion completo dado el `path` (lista ordenada de
/// `PathHopSecret`, primer elemento = primer hop, último = destino
/// final). Devuelve el `OnionWire` con todo lo que el caller necesita.
pub fn build_onion(path: &[PathHopSecret], body: Vec<u8>) -> Result<OnionWire> {
    if path.is_empty() {
        return Err(OrrError::Relay("onion: empty path".into()));
    }

    // Capa más interna: cifra body con K_{O,dst}.
    let dst = path.last().unwrap();
    let kid_dst = *Uuid::new_v4().as_bytes();
    let mut current_xor = xor_with_derived_key(&dst.master_secret, &kid_dst, &body);
    let mut current_next_orr_id = dst.orr_id.clone();
    let mut current_key_id = kid_dst;

    // Reenvolver hacia fuera: cada hop intermedio recibe una capa cuyo
    // plaintext es un `InnerLayer { next_orr_id, key_id, xor_ct }`
    // apuntando a la siguiente capa.
    for i in (0..path.len() - 1).rev() {
        let hop = &path[i];
        let layer = InnerLayer {
            next_orr_id: current_next_orr_id,
            key_id:      current_key_id,
            xor_ct:      current_xor,
        };
        let layer_pt = layer.encode()?;
        let kid_hop = *Uuid::new_v4().as_bytes();
        current_xor = xor_with_derived_key(&hop.master_secret, &kid_hop, &layer_pt);
        current_next_orr_id = hop.orr_id.clone();
        current_key_id = kid_hop;
    }

    Ok(OnionWire {
        first_hop_orr: current_next_orr_id,
        first_key_id:  current_key_id,
        max_hops:      (path.len() as i32) - 1,
        payload:       current_xor,
    })
}

/// Pela una capa.
///
/// `master_secret`: el shared secret de 32 B con `header.from` (lookup
/// en `PeerRegistry::master_secret`).
/// `key_id`: del `header.key_id` del wire frame entrante.
/// `xor_ct`: el `payload` del wire frame entrante (ya descifrado por el
/// QKC en el último hop QKC-OTP).
/// `max_hops`: del `header.max_hops` del wire frame entrante. Si > 0
/// devuelve `Forward(InnerLayer)`; si == 0 devuelve `Deliver(body)`.
pub fn peel(
    master_secret: &[u8; 32],
    key_id: &[u8; 16],
    xor_ct: &[u8],
    max_hops: i32,
) -> Result<Peeled> {
    let pt = xor_with_derived_key(master_secret, key_id, xor_ct);
    if max_hops <= 0 {
        Ok(Peeled::Deliver(pt))
    } else {
        let layer = InnerLayer::decode(&pt)?;
        Ok(Peeled::Forward(layer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    fn random_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut s);
        s
    }

    #[test]
    fn hkdf_deterministic() {
        let ms = [0x42; 32];
        let kid = [0xAB; 16];
        let k1 = derive_key(&ms, &kid, 64);
        let k2 = derive_key(&ms, &kid, 64);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 64);
    }

    #[test]
    fn hkdf_different_kid_different_key() {
        let ms = [0x42; 32];
        let kid1 = [0xAB; 16];
        let kid2 = [0xCD; 16];
        let k1 = derive_key(&ms, &kid1, 64);
        let k2 = derive_key(&ms, &kid2, 64);
        assert_ne!(k1, k2);
    }

    #[test]
    fn hkdf_different_len_different_prefix() {
        // El info incluye body_len, así que K(L1) y K(L2) no son
        // truncamientos uno del otro.
        let ms = [0x42; 32];
        let kid = [0xAB; 16];
        let k_64 = derive_key(&ms, &kid, 64);
        let k_128 = derive_key(&ms, &kid, 128);
        assert_ne!(k_64[..], k_128[..64]);
    }

    #[test]
    fn hkdf_large_body_chunking() {
        // Verifica que body > HKDF_MAX_OUTPUT funciona correctamente.
        let ms = [0x42; 32];
        let kid = [0xAB; 16];
        let big_len = HKDF_MAX_OUTPUT + 1000;
        let k = derive_key(&ms, &kid, big_len);
        assert_eq!(k.len(), big_len);
        // El XOR round-trip funciona también con bodies grandes.
        let big_body = vec![0x77u8; big_len];
        let ct = xor_with_derived_key(&ms, &kid, &big_body);
        let pt = xor_with_derived_key(&ms, &kid, &ct);
        assert_eq!(pt, big_body);
    }

    #[test]
    fn xor_round_trip() {
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let body = b"hola mundo onion v3".to_vec();
        let ct = xor_with_derived_key(&ms, &kid, &body);
        let pt = xor_with_derived_key(&ms, &kid, &ct);
        assert_eq!(pt, body);
    }

    #[test]
    fn build_and_peel_1_hop() {
        let ms_dst = random_secret();
        let body = b"single hop test".to_vec();
        let path = vec![PathHopSecret {
            orr_id:        "orr_dst".into(),
            master_secret: ms_dst,
        }];
        let onion = build_onion(&path, body.clone()).unwrap();
        assert_eq!(onion.first_hop_orr, "orr_dst");
        assert_eq!(onion.max_hops, 0);
        let peeled = peel(&ms_dst, &onion.first_key_id, &onion.payload, 0).unwrap();
        assert_eq!(peeled, Peeled::Deliver(body));
    }

    #[test]
    fn build_and_peel_3_hops() {
        let ms_b = random_secret();
        let ms_c = random_secret();
        let ms_d = random_secret();
        let body = b"viaje cebolla 3 hops".to_vec();
        let path = vec![
            PathHopSecret { orr_id: "orr_b".into(), master_secret: ms_b },
            PathHopSecret { orr_id: "orr_c".into(), master_secret: ms_c },
            PathHopSecret { orr_id: "orr_d".into(), master_secret: ms_d },
        ];
        let onion = build_onion(&path, body.clone()).unwrap();
        assert_eq!(onion.first_hop_orr, "orr_b");
        assert_eq!(onion.max_hops, 2);

        // B pela: max_hops=2 ⇒ Forward(InnerLayer apuntando a C).
        let peeled_b = peel(&ms_b, &onion.first_key_id, &onion.payload, 2).unwrap();
        let inner_b = match peeled_b {
            Peeled::Forward(l) => l,
            _ => panic!("expected Forward"),
        };
        assert_eq!(inner_b.next_orr_id, "orr_c");

        // C pela: max_hops=1 ⇒ Forward(InnerLayer apuntando a D).
        let peeled_c = peel(&ms_c, &inner_b.key_id, &inner_b.xor_ct, 1).unwrap();
        let inner_c = match peeled_c {
            Peeled::Forward(l) => l,
            _ => panic!("expected Forward"),
        };
        assert_eq!(inner_c.next_orr_id, "orr_d");

        // D pela: max_hops=0 ⇒ Deliver(body_dkms).
        let peeled_d = peel(&ms_d, &inner_c.key_id, &inner_c.xor_ct, 0).unwrap();
        assert_eq!(peeled_d, Peeled::Deliver(body));
    }

    #[test]
    fn build_2_hops_size_growth_is_linear() {
        // body = 64 B, esperado outer ≈ 64 + 45 = ~110 B.
        let ms_x = random_secret();
        let ms_dst = random_secret();
        let body = vec![0xAA; 64];
        let path = vec![
            PathHopSecret { orr_id: "orr_x".into(), master_secret: ms_x },
            PathHopSecret { orr_id: "orr_dst".into(), master_secret: ms_dst },
        ];
        let onion = build_onion(&path, body).unwrap();
        // Tamaño realista: chequeo de cota superior — no debería estar
        // por debajo de 64 ni explotar a > 200 B.
        assert!(onion.payload.len() >= 64);
        assert!(onion.payload.len() < 200,
            "outer payload = {} B (esperado <200)", onion.payload.len());
    }

    #[test]
    fn empty_path_fails() {
        let body = vec![1, 2, 3];
        assert!(build_onion(&[], body).is_err());
    }

    #[test]
    fn wrong_secret_yields_garbage_not_panic() {
        let ms = random_secret();
        let wrong = random_secret();
        let body = b"x".to_vec();
        let path = vec![PathHopSecret {
            orr_id:        "orr_dst".into(),
            master_secret: ms,
        }];
        let onion = build_onion(&path, body.clone()).unwrap();
        let peeled = peel(&wrong, &onion.first_key_id, &onion.payload, 0).unwrap();
        if let Peeled::Deliver(p) = peeled {
            assert_ne!(p, body);
        } else {
            panic!("expected Deliver");
        }
    }
}
