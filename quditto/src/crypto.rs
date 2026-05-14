//! Generación de claves aleatorias rápida.
//!
//! El tamaño se decide por config (`--key-size-bits`). El material se
//! aloja en `Vec<u8>` para soportar cualquier múltiplo de 8 bits sin
//! recompilar. El RNG criptográfico se inyecta para evitar syscalls
//! en el hot path: el caller construye un `ChaCha20Rng` (seeded desde
//! `OsRng`) y lo reusa.

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use uuid::Uuid;

/// Una clave fresca con su identificador.
#[derive(Debug, Clone)]
pub struct Key {
    pub key_id: Uuid,
    pub material: Vec<u8>,
}

impl Key {
    /// Mintea una clave de `n_bytes` con su UUID v4 (ambos del mismo
    /// stream del RNG → una sola pasada).
    #[inline]
    pub fn mint(rng: &mut ChaCha20Rng, n_bytes: usize) -> Self {
        // 16 B (UUID) + n_bytes (material) en una sola fill.
        let mut buf = vec![0u8; 16 + n_bytes];
        rng.fill_bytes(&mut buf);

        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&buf[..16]);
        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40; // versión v4
        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80; // variante RFC 4122

        let material = buf[16..].to_vec();
        Self { key_id: Uuid::from_bytes(uuid_bytes), material }
    }
}

/// RNG userspace seeded una vez con `OsRng`. CSPRNG suficientemente
/// rápido para minar millones de claves/s sin syscalls.
pub fn build_rng() -> ChaCha20Rng {
    ChaCha20Rng::from_entropy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_produces_unique_ids() {
        let mut r = build_rng();
        let a = Key::mint(&mut r, 32);
        let b = Key::mint(&mut r, 32);
        assert_ne!(a.key_id, b.key_id);
        assert_eq!(a.material.len(), 32);
        assert_eq!(b.material.len(), 32);
    }

    #[test]
    fn mint_random_for_1024_bits() {
        let mut r = build_rng();
        let a = Key::mint(&mut r, 128);
        let b = Key::mint(&mut r, 128);
        assert_eq!(a.material.len(), 128);
        assert_ne!(a.material, b.material);
    }

    #[test]
    fn uuid_has_v4_version_and_variant_bits() {
        let mut r = build_rng();
        for _ in 0..100 {
            let k = Key::mint(&mut r, 32);
            let bytes = k.key_id.as_bytes();
            assert_eq!(bytes[6] & 0xf0, 0x40);
            assert_eq!(bytes[8] & 0xc0, 0x80);
        }
    }
}
