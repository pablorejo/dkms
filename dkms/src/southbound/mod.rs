//! Clientes gRPC del DKMS hacia el plano de control / transporte.
//!
//! * [`sdn`] — `SdnControl` (rutas, admisión, SAE binding, métricas).
//! * [`qkc`] — `QkcControl` (reserve/release de claves de transporte).
//! * [`orr`] — `OrrControl` (transporte de mensajes vía ORR↔QKC).
//!   Opcional: presente solo si `southbound.orr_endpoint` está
//!   configurado. Hoy NO se enchufa en el flujo principal; sirve para
//!   que un futuro cambio en `peer_client.rs` lo use como transporte
//!   alternativo a HTTP/2 ETSI 020.

pub mod orr;
pub mod qkc;
pub mod sdn;
pub mod sdn_http;

pub use orr::OrrClient;
pub use qkc::QkcClient;
pub use sdn::SdnClient;
pub use sdn_http::{
    CommodityDemand, DemandApplied, DemandReport, DkmsRatesResponse, PeerRate, SdnHttpClient,
};
