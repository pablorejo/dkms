#![forbid(unsafe_code)]
// El error principal (`DkmsError`) lleva variantes que envuelven errores
// gordos de `common`/`tonic` (`Transport`, `Status`...). Cambiar a
// `Box<inner>` rompería la ergonomía de `?` con `#[from]`; en este crate
// preferimos pagar 176 B en el stack del `Result` que añadir indirección
// en toda la API.
#![allow(clippy::result_large_err)]

//! DKMS — Distributed Key Management Service.
//!
//! El módulo que ven los clientes. Sirve ETSI GS QKD 014 a los SAEs
//! ([`etsi_http::v014`], mTLS con la CA de SAEs) y ETSI GS QKD 020 a los
//! DKMS de los demás nodos ([`etsi_http::v020`], HTTP/2 + mTLS con la CA
//! de red), y es el extremo criptográfico de la relación entre dos nodos.
//!
//! Dos clases de material, en dos caminos distintos:
//!
//! * **Claves de transporte** — llenan `buffer_enc[peer]` aquí y su gemelo
//!   `buffer_dec[yo]` en el peer ([`state`]). Las genera el [`control`]
//!   (generator) al ritmo que dicta la SDN (`GET /rate`) y las manda por
//!   el ORR co-localizado ([`southbound::orr`]), que las lleva por la red
//!   de QKCs. Cada `DKMS_BUFFER` va sellado extremo a extremo con
//!   AES-256-GCM bajo un secreto ML-KEM por par ([`e2e`]) acordado sobre
//!   el propio canal ETSI-020; el ORR y los QKC sólo lo transportan. El
//!   receptor confirma por ETSI-020 (`ext_keys/ack`).
//! * **Claves de sesión** — las que pide un SAE con `enc_keys`. Se generan
//!   aquí, se envuelven en OTP con una clave de transporte y viajan al DKMS
//!   del SAE esclavo por ETSI-020 (`ext_keys`); el esclavo las recoge con
//!   `dec_keys`. Si un destino falla, se reembolsan los tokens del SAE.
//!
//! Todo el material vive sólo en memoria y se zeroiza al soltar: no hay
//! disco. Un reinicio deja los buffers vacíos y los peers lo detectan por
//! la `incarnation` que viaja en cada `DKMS_BUFFER`, descartando su mitad
//! y dejando que el generator rellene.
//!
//! Hacia el plano de control: el DKMS se anuncia a la SDN en bucle con su
//! ORR y sus SAEs ([`southbound::sdn_announce`]), le reporta demanda
//! (`POST /demand`, [`demand_tracker`]) y le consulta dónde vive cada SAE
//! ([`sae_binding`]); la respuesta al anuncio trae el conjunto de pares
//! DKMS, que cambia en caliente ([`peers`]). La superficie gRPC
//! `DkmsControl` ([`grpc_server`]) es sólo de gestión y escucha en
//! loopback.

pub mod admission;
pub mod config;
pub mod control;
pub mod demand_tracker;
pub mod e2e;
pub mod error;
pub mod etsi_http;
pub mod grpc_server;
pub mod peer_client;
pub mod peers;
pub mod sae_binding;
pub mod security_level;
pub mod service;
pub mod southbound;
pub mod state;
pub mod token_bucket;
