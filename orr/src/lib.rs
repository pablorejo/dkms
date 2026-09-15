#![forbid(unsafe_code)]
// OrrError envuelve io::Error/anyhow::Error (~176 B) por #[from], lo que
// dispara `result_large_err` en cada Result<(), OrrError>. Boxearlos sería
// churn cosmético — los retornos no van por hot paths.
#![allow(clippy::result_large_err)]

//! ORR — Onion Routing Router.
//!
//! Capa entre el DKMS (gRPC `OrrControl`, mTLS por defecto, [`grpc_tls`]) y
//! el QKC co-localizado (TCP binario `wire`, [`qkc_link`]). El ORR no
//! enruta: recibe payloads del DKMS (`SendMessage`), los empuja al QKC con
//! la cabecera ORR ([`header`]) y el QKC los lleva por la red; lo que el
//! QKC entrega para este nodo se reparte a los suscriptores de
//! `StreamDeliveries` ([`service`]).
//!
//! Cuatro modos según `max_hops` ([`onion`]):
//!
//!   * `max_hops == 0`  → passthrough (**el default**): el ORR es un relé.
//!     El sello extremo a extremo del material lo pone el DKMS, no el ORR.
//!   * `max_hops == 1`  → una capa con el ORR destino.
//!   * `max_hops == -1` → una capa por ORR del camino que da la SDN.
//!   * `max_hops >= 2`  → cebolla truncada a `max_hops` capas.
//!
//! Los modos con capas son privacidad de camino opcional sobre un payload
//! que ya llega sellado. Cada capa es AES-256-GCM con clave derivada por
//! HKDF de un `master_secret` por par de ORRs; el tag viaja en la cabecera,
//! nunca en el payload (cada byte de payload cuesta material QKD en los
//! enlaces OTP). El `master_secret` se acuerda con ML-KEM contra la
//! identidad del peer ([`identity`], [`bootstrap`]) y rota por épocas
//! ([`rotation`]); las RPCs de rotación van autenticadas con HMAC
//! ([`macs`]) y la identidad anunciada va firmada con el certificado de
//! nodo.
//!
//! Como el resto de módulos, el ORR se anuncia a la SDN en bucle
//! ([`sdn_announce`]) anclado a su QKC, y la respuesta trae sus pares.

pub mod bootstrap;
pub mod config;
pub mod dkms_header;
pub mod error;
pub mod grpc_server;
pub mod grpc_tls;
pub mod handshake;
pub mod header;
pub mod identity;
pub mod macs;
pub mod onion;
pub mod onion_replay;
pub mod peers;
pub mod qkc_link;
pub mod relay;
pub mod rotation;
pub mod sdn_announce;
pub mod sdn_client;
pub mod service;
pub mod stats;
