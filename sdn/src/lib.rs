//! SDN = Software Defined Network module.
//!
//! Owns the in-memory topology graph for the entire QKD deployment,
//! computes routes (shortest path / min-cost-flow), runs link admission
//! control, and propagates changes to interested parties (DKMS, QKC, ORR)
//! via streaming gRPC.

// SdnError wraps io::Error, serde_json::Error and anyhow::Error (each
// ~176 B), so the Err-variant is unavoidably large. Boxing every #[from]
// variant would be cosmetic churn — control-plane fns don't run in hot
// paths and Result<(), SdnError> isn't passed around in tight loops.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod debounce;
pub mod demand;
pub mod error;
pub mod grpc_server;
pub mod http_api;
pub mod link_admission;
pub mod mcf;
pub mod mcmcf;
pub mod metrics;
pub mod push;
pub mod routing;
pub mod service;
pub mod topology;
