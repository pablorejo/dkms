//! Clientes gRPC del DKMS hacia el plano de control.
//!
//! * [`sdn`] — `SdnControl` (rutas, admisión, SAE binding, métricas).
//! * [`qkc`] — `QkcControl` (reserve/release de claves de transporte).
//!
//! El DKMS **no** habla con ORR ni con quditto directamente.

pub mod qkc;
pub mod sdn;

pub use qkc::QkcClient;
pub use sdn::SdnClient;
