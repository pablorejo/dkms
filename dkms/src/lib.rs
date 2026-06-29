// El error principal (`DkmsError`) lleva variantes que envuelven errores
// gordos de `common`/`tonic` (`Transport`, `Status`...). Cambiar a
// `Box<inner>` rompería la ergonomía de `?` con `#[from]`; en este crate
// preferimos pagar 176 B en el stack del `Result` que añadir indirección
// en toda la API.
#![allow(clippy::result_large_err)]

//! DKMS — Distributed Key Management Service.
//!
//! Sirve ETSI GS QKD 014 al sur (SAEs vía mTLS, [`etsi_http::v014`]) y
//! ETSI GS QKD 020 entre DKMSs vecinos (HTTP/2 + mTLS,
//! [`etsi_http::v020`]) para distribuir multicast de claves de sesión.
//! Habla gRPC con SDN y QKC ([`southbound`]).
//!
//! Decisiones de diseño respecto al DKMS Python original:
//!
//! * Sin ORR en el plano DKMS. ORR queda para QKC.
//! * Buffers de claves de transporte sólo en memoria, zeroizados al
//!   liberar.  No hay persistencia; al reiniciar se regeneran desde QKC.
//! * Política de fallo "todo-o-nada" en el `enc_keys` multicast: si un
//!   destino falla, se reembolsan los tokens del SAE master.

pub mod admission;
pub mod config;
pub mod control;
pub mod demand_tracker;
pub mod error;
pub mod etsi_http;
pub mod grpc_server;
pub mod peer_client;
pub mod sae_binding;
pub mod security_level;
pub mod service;
pub mod southbound;
pub mod state;
pub mod token_bucket;
