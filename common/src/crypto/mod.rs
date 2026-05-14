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
pub mod otp;
pub mod pqc;
