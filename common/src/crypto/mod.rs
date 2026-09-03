//! Shared crypto primitives used by more than one module.
//!
//! Módulos:
//!   * [`otp`]  — One-time-pad sobre material QKD (XOR salto-a-salto).
//!   * [`pqc`]  — ML-KEM (FIPS 203) para acuerdos de clave PQC.
//!   * [`aead`] — AES-256-GCM como DEM en el patrón KEM-DEM.
//!
//! Módulo local de cada crate (p.ej. el KME del QKC, el wrap ETSI del
//! DKMS) vive en su propio sitio; aquí está sólo lo común.

pub mod aead;
pub mod frame_mac;
pub mod link_mac;
pub mod otp;
pub mod pqc;
pub mod pqc_sign;

/// Comparación en tiempo constante de dos slices (misma longitud y mismo
/// contenido). Para huellas y tags que viajan en claro dentro de un canal
/// autenticado no hay oráculo que explotar hoy, pero un `==` sobre bytes
/// secretos es exactamente lo que no debe existir en un camino de claves
/// (auditoría 2026-09-03, B-12). Sin `subtle`: un OR acumulado sobre XOR
/// que el optimizador no puede cortocircuitar (`black_box`).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut acc: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        acc |= std::hint::black_box(x ^ y);
    }
    std::hint::black_box(acc) == 0
}

#[cfg(test)]
mod ct_eq_tests {
    #[test]
    fn ct_eq_compara_bien() {
        assert!(super::ct_eq(b"abc", b"abc"));
        assert!(!super::ct_eq(b"abc", b"abd"));
        assert!(!super::ct_eq(b"abc", b"ab"));
        assert!(super::ct_eq(b"", b""));
    }
}
