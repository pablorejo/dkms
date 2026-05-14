//! QKC = Quantum Key Channel.
//!
//! Per-link key transport between adjacent nodes. The control plane is
//! exposed over gRPC (`grpc_server`) — used by SDN and DKMS — while the
//! actual key-bearing frames travel over a custom binary TCP wire
//! (`socket_server` + `socket_client`) for latency reasons.
//!
//! Key concepts:
//! - [`kme::Kme`] — Key Management Entity. Owns a local key buffer per peer.
//! - [`crypto_engine::CryptoEngine`] — wraps/unwraps payloads using OTP from
//!   the local KME.
//! - [`routing::RoutingResolver`] — given a (src, dst, sae) tuple, decide
//!   which peer to forward to next.
//! - [`token_bucket::TokenBucket`] — per-link admission control.
//! - [`service::QkcService`] — holds the above and is shared with both the
//!   gRPC server and the TCP server.

pub mod config;
pub mod crypto_engine;
pub mod error;
pub mod grpc_server;
pub mod kme;
pub mod message_models;
pub mod routing;
pub mod service;
pub mod socket_client;
pub mod socket_server;
pub mod token_bucket;
