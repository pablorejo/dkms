//! Generación de claves aleatorias rápida.
//!
//! La spec del proyecto fija 256 bits → usamos `[u8; 32]` inline (sin
//! heap por clave). El RNG criptográfico se inyecta para que el hot
//! path no tenga ni un syscall por mint: el caller construye un
//! `ChaCha20Rng` una sola vez (seeded desde `OsRng`) y lo reusa.

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use uuid::Uuid;

/// Tamaño fijo de las claves (spec del proyecto).
pub const KEY_SIZE_BITS: u32 = 256;
pub const KEY_SIZE_BYTES: usize = (KEY_SIZE_BITS / 8) as usize;

/// Una clave fresca con su identificador. Cabe en stack (48 B).
#[derive(Debug, Clone, Copy)]
pub struct Key {
    pub key_id: Uuid,
    pub material: [u8; KEY_SIZE_BYTES],
}

impl Key {
    /// Construye una clave nueva usando el RNG dado. No hace allocs.
    #[inline]
    pub fn mint(rng: &mut ChaCha20Rng) -> Self {
        // 16 B del UUID + 32 B del material = 48 B random en una sola
        // pasada por el stream, sin syscalls.
        let mut buf = [0u8; 48];
        rng.fill_bytes(&mut buf);

        let mut uuid_bytes = [0u8; 16];
        uuid_bytes.copy_from_slice(&buf[..16]);
        // RFC 4122 v4: set version + variant bits.
        uuid_bytes[6] = (uuid_bytes[6] & 0x0f) | 0x40;
        uuid_bytes[8] = (uuid_bytes[8] & 0x3f) | 0x80;

        let mut material = [0u8; KEY_SIZE_BYTES];
        material.copy_from_slice(&buf[16..48]);

        Self {
            key_id: Uuid::from_bytes(uuid_bytes),
            material,
        }
    }
}

/// Construye un RNG seeded con `OsRng` (una vez). El `ChaCha20Rng`
/// resultante es CSPRNG y muchísimo más rápido que llamar a `OsRng`
/// por cada clave.
pub fn build_rng() -> ChaCha20Rng {
    ChaCha20Rng::from_entropy()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_produces_unique_ids() {
        let mut r = build_rng();
        let a = Key::mint(&mut r);
        let b = Key::mint(&mut r);
        assert_ne!(a.key_id, b.key_id);
        assert_eq!(a.material.len(), 32);
    }

    #[test]
    fn mint_produces_random_bytes() {
        let mut r = build_rng();
        let a = Key::mint(&mut r);
        let b = Key::mint(&mut r);
        assert_ne!(a.material, b.material);
    }

    #[test]
    fn uuid_has_v4_version_and_variant_bits() {
        let mut r = build_rng();
        for _ in 0..100 {
            let k = Key::mint(&mut r);
            let bytes = k.key_id.as_bytes();
            assert_eq!(bytes[6] & 0xf0, 0x40, "version nibble must be 4");
            assert_eq!(bytes[8] & 0xc0, 0x80, "variant must be RFC 4122");
        }
    }
}
