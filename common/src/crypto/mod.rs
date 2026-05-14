//! Shared crypto primitives used by more than one module.
//!
//! Module-local crypto (e.g. QKC's KME, DKMS's ETSI key wrapping) lives in
//! its own module; this is just the lowest common denominator.

pub mod otp;
pub mod pqc;
