//! Clientes del DKMS hacia el plano de control y el transporte.
//!
//! * [`sdn`] — `SdnControl` por gRPC (SAE binding, topología en stream).
//! * [`sdn_http`] — el admin HTTP de la SDN: `GET /rate/{dkms_id}` (las
//!   tasas por peer que alimentan el generator) y `POST /demand`.
//! * [`sdn_announce`] — el bucle `POST /register/dkms`, que es también el
//!   heartbeat y trae de vuelta el conjunto de pares.
//! * [`orr`] — `OrrControl` por gRPC (mTLS por defecto): el transporte de
//!   las claves de transporte hacia los buffers de los peers. Opcional
//!   (`southbound.orr_endpoint`); sin él el generator no arranca y las
//!   claves de sesión siguen saliendo por ETSI 020, pero sin material que
//!   las envuelva.

pub mod orr;
pub mod sdn;
pub mod sdn_announce;
pub mod sdn_http;

pub use orr::OrrClient;
pub use sdn::SdnClient;
pub use sdn_http::{
    CommodityDemand, DemandApplied, DemandReport, DkmsRatesResponse, PeerRate, SdnHttpClient,
};
