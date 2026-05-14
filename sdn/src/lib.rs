//! SDN = Software Defined Network module.
//!
//! Owns the in-memory topology graph for the entire QKD deployment,
//! computes routes (shortest path / min-cost-flow), runs link admission
//! control, and propagates changes to interested parties (DKMS, QKC, ORR)
//! via streaming gRPC.

pub mod config;
pub mod debounce;
pub mod error;
pub mod grpc_server;
pub mod http_api;
pub mod link_admission;
pub mod mcf;
pub mod push;
pub mod routing;
pub mod service;
pub mod topology;
