//! quditto — simulador de enlace QKD.
//!
//! Mantiene un **buffer FIFO de claves aleatorias de 256 bits** que se
//! llena en background a tasa
//!
//! ```text
//!   R(d) = R₀ · 10^(-α · d / 10)   keys/s
//! ```
//!
//! Sirve las claves por **HTTP ETSI GS QKD 014**:
//!
//! * `GET /api/v1/keys/{sae_id}/enc_keys` → claves frescas con su `key_ID`.
//! * `GET /api/v1/keys/{sae_id}/dec_keys?key_ID=…` → recupera por ID.
//! * `GET /api/v1/keys/{sae_id}/status` → estado del buffer.
//!
//! Los modelos de wire vienen del crate [`etsi`] (port 1:1 del
//! Python `ETSIQKD/`).

pub mod config;
pub mod crypto;
pub mod error;
pub mod link;
pub mod server;
pub mod service;
pub mod tls_server;
