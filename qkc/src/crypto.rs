//! Cifrado OTP multi-chunk.
//!
//! Cada chunk del plaintext usa una clave OTP distinta de
//! `key_size_bits` bits. La clave se obtiene del quditto compartido del
//! enlace; el caller pasa las claves (en orden) — esta función solo
//! hace el XOR puro, sin tocar la red.
//!
//! Para 256 bits eso son 32 bytes por chunk. El plaintext se trocea en
//! chunks de 32 B; el último chunk puede ser parcial y solo consume
//! los bytes que necesita de la última clave.

use uuid::Uuid;

use crate::{error::QkcError, kme::OtpKey};

/// Devuelve el número de chunks necesarios para un plaintext de
/// `n_bytes`, con tamaño de chunk `chunk_bytes` (`key_size_bits / 8`).
#[inline]
pub fn num_chunks(n_bytes: usize, chunk_bytes: usize) -> usize {
    if n_bytes == 0 {
        0
    } else {
        n_bytes.div_ceil(chunk_bytes)
    }
}

/// Cifra `plaintext` con la secuencia de claves. Devuelve los bytes
/// del ciphertext (misma longitud que el plaintext) y la lista de
/// `key_id`s usados, en orden.
///
/// Requiere `keys.len() >= num_chunks(plaintext.len(), key_bytes)`. Si
/// hay menos claves, devuelve error.
pub fn encrypt(plaintext: &[u8], keys: &[OtpKey]) -> Result<(Vec<u8>, Vec<Uuid>), QkcError> {
    if plaintext.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if keys.is_empty() {
        return Err(QkcError::BadRequest("encrypt called with 0 keys".into()));
    }
    let chunk_bytes = keys[0].material.len();
    if chunk_bytes == 0 {
        return Err(QkcError::BadRequest(
            "encrypt: key material is empty".into(),
        ));
    }
    let needed = num_chunks(plaintext.len(), chunk_bytes);
    if keys.len() < needed {
        return Err(QkcError::NotEnoughKeys {
            requested: needed as u32,
            received: keys.len() as u32,
        });
    }

    let mut out = Vec::with_capacity(plaintext.len());
    let mut ids = Vec::with_capacity(needed);
    for (i, chunk) in plaintext.chunks(chunk_bytes).enumerate() {
        let k = &keys[i];
        for (b, kb) in chunk.iter().zip(k.material.iter()) {
            out.push(b ^ kb);
        }
        ids.push(k.key_id);
    }
    Ok((out, ids))
}

/// Descifra `ciphertext` con la secuencia de claves indicada en
/// `key_ids_order` y devueltas por `keys` (en cualquier orden — esta
/// función las re-ordena por `key_id`).
///
/// `chunk_bytes` debe coincidir con `key_size_bits / 8`.
pub fn decrypt(
    ciphertext: &[u8],
    key_ids_order: &[Uuid],
    keys: &[OtpKey],
    chunk_bytes: usize,
) -> Result<Vec<u8>, QkcError> {
    if ciphertext.is_empty() {
        return Ok(Vec::new());
    }
    if key_ids_order.is_empty() {
        return Err(QkcError::BadRequest("decrypt: no key_ids in header".into()));
    }
    if chunk_bytes == 0 {
        return Err(QkcError::BadRequest("decrypt: chunk_bytes == 0".into()));
    }
    let needed = num_chunks(ciphertext.len(), chunk_bytes);
    if key_ids_order.len() < needed {
        return Err(QkcError::BadRequest(format!(
            "decrypt: header lists {} key_ids but {} chunks needed",
            key_ids_order.len(),
            needed
        )));
    }

    // Buscar sin asumir orden. El caso de diseño es 1 clave por frame
    // (payload de 32 B): montar un HashMap era una alloc + SipHash por frame
    // para un único lookup. Con pocas claves un barrido lineal gana; el mapa
    // queda para batches grandes.
    let by_id = if keys.len() > 8 {
        let mut m = std::collections::HashMap::with_capacity(keys.len());
        for k in keys {
            m.insert(k.key_id, &k.material[..]);
        }
        Some(m)
    } else {
        None
    };
    let find = |id: &Uuid| -> Option<&[u8]> {
        match &by_id {
            Some(m) => m.get(id).copied(),
            None => keys
                .iter()
                .find(|k| k.key_id == *id)
                .map(|k| &k.material[..]),
        }
    };

    let mut out = Vec::with_capacity(ciphertext.len());
    for (i, chunk) in ciphertext.chunks(chunk_bytes).enumerate() {
        let id = &key_ids_order[i];
        let mat =
            find(id).ok_or_else(|| QkcError::Quditto(format!("missing key for key_id {id}")))?;
        if mat.len() < chunk.len() {
            return Err(QkcError::Quditto(format!(
                "key for {id} is shorter than chunk ({} < {})",
                mat.len(),
                chunk.len()
            )));
        }
        for (b, kb) in chunk.iter().zip(mat.iter()) {
            out.push(b ^ kb);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_key(idx: u8) -> OtpKey {
        OtpKey {
            key_id: Uuid::from_u128(idx as u128),
            material: zeroize::Zeroizing::new(vec![idx; 32]),
        }
    }

    #[test]
    fn encrypt_then_decrypt_round_trip() {
        let pt = b"hello world, this is a longer test message that spans multiple 32-byte chunks!"
            .to_vec();
        let keys: Vec<_> = (1..=4).map(fake_key).collect();
        let (ct, ids) = encrypt(&pt, &keys).unwrap();
        assert_eq!(ct.len(), pt.len());
        let pt2 = decrypt(&ct, &ids, &keys, 32).unwrap();
        assert_eq!(pt, pt2);
    }

    #[test]
    fn encrypt_rejects_when_too_few_keys() {
        let pt = vec![0u8; 100]; // necesita 4 chunks de 32
        let keys: Vec<_> = (1..=2).map(fake_key).collect();
        let err = encrypt(&pt, &keys).unwrap_err();
        match err {
            QkcError::NotEnoughKeys {
                requested,
                received,
            } => {
                assert_eq!(requested, 4);
                assert_eq!(received, 2);
            }
            other => panic!("wrong error: {other:?}"),
        }
    }

    #[test]
    fn decrypt_finds_keys_in_any_order() {
        let pt = vec![0xab; 80];
        let keys: Vec<_> = (1..=3).map(fake_key).collect();
        let (ct, ids) = encrypt(&pt, &keys).unwrap();
        // Pasamos las keys revertidas — debe seguir descifrando bien.
        let mut keys_rev = keys.clone();
        keys_rev.reverse();
        let pt2 = decrypt(&ct, &ids, &keys_rev, 32).unwrap();
        assert_eq!(pt, pt2);
    }

    /// Con >8 claves entra el camino del HashMap (el lineal cubre el caso de
    /// diseño de 1 clave/frame): mismo resultado con cualquier orden.
    #[test]
    fn decrypt_hash_path_with_many_keys() {
        let pt = vec![0x5c; 12 * 32];
        let keys: Vec<_> = (1..=12).map(fake_key).collect();
        let (ct, ids) = encrypt(&pt, &keys).unwrap();
        let mut keys_rev = keys.clone();
        keys_rev.reverse();
        assert_eq!(decrypt(&ct, &ids, &keys_rev, 32).unwrap(), pt);
    }
}
