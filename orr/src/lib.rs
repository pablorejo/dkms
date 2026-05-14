//! ORR = Onion Routing Router.
//!
//! Builds onion circuits using PQC KEMs for the per-hop keys and forwards
//! frames hop-by-hop. The actual key-bearing traffic between adjacent
//! nodes is owned by [`qkc`]; ORR sits on top of that to provide
//! end-to-end confidentiality across multi-hop paths.

pub mod config;
pub mod error;
pub mod grpc_server;
pub mod handshake;
pub mod peers;
pub mod relay;
pub mod service;
