// OrrError envuelve io::Error/anyhow::Error (~176 B) por #[from], lo que
// dispara `result_large_err` en cada Result<(), OrrError>. Boxearlos sería
// churn cosmético — los retornos no van por hot paths.
#![allow(clippy::result_large_err)]

//! ORR — Onion Routing Router.
//!
//! Capa entre el DKMS (gRPC, `OrrControl`) y el QKC co-localizado
//! (TCP binario `wire`). El ORR no mantiene routing propio: recibe
//! payloads del DKMS, los empuja al QKC con la cabecera ORR
//! correspondiente, y procesa los entrantes del QKC entregándolos a
//! los suscriptores `StreamDeliveries`.
//!
//! Cuatro modos según `max_hops` (paper §5.1):
//!   * `max_hops == 0`  → passthrough; el ORR no añade capas.
//!   * `max_hops == 1`  → PQC end-to-end con el ORR destino (1 capa onion).
//!   * `max_hops == -1` → cebolla PQC capa-a-capa por todo el path SDN.
//!   * `max_hops >= 2`  → cebolla PQC truncada a `max_hops` capas.
//!
//! Modelo cripto: ML-KEM (FIPS 203) para acordar shared secrets por hop,
//! XOR-cipher ("OTP-style") por capa usando los secrets concatenados.
//! Ver [`onion`] para el formato exacto.

pub mod bootstrap;
pub mod config;
pub mod dkms_header;
pub mod error;
pub mod grpc_server;
pub mod handshake;
pub mod header;
pub mod identity;
pub mod macs;
pub mod onion;
pub mod peers;
pub mod qkc_link;
pub mod relay;
pub mod rotation;
pub mod sdn_announce;
pub mod sdn_client;
pub mod service;
pub mod stats;
