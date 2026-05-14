//! Estado en memoria del DKMS — todo zeroize-aware, sin disco.
//!
//! * [`buffer`]  — buffer FIFO de claves de transporte (un par ENC/DEC por
//!   peer DKMS).
//! * [`pool`]    — agregador `peer → (enc, dec)`.
//! * [`pending`] — claves de sesión a la espera de los SAEs autorizados.

pub mod buffer;
pub mod pending;
pub mod pool;

pub use buffer::{SecureKeyBuffer, TransportKey};
pub use pending::{PendingEntry, PendingStore};
pub use pool::{BufferPool, PeerBuffers};
