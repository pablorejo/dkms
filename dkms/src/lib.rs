//! DKMS = Distributed Key Management Service.
//!
//! The customer-facing module: speaks ETSI QKD 014 / 020 to SAEs over
//! HTTP(S) and orchestrates calls to ORR (path setup), QKC (key
//! reservations) and SDN (path computation) on the back-end.
//!
//! Modules:
//! - [`http_server`]      — axum HTTP server for SAEs (ETSI 014 / 020
//!   handlers will plug in from the `etsi` crate when DKMS is rewritten).
//! - [`buffer`]           — buffered key store per (sae_local, sae_remote).
//! - [`scheduler`]        — round-robin draining of remote deliveries.
//! - [`token_bucket`]     — per-SAE rate limit.
//! - [`orr_client`], [`sdn_client`], [`qkc_client`] — typed gRPC clients.
//! - [`qrng_adapter`]     — pull randomness from a local QRNG/quditto.
//! - [`agent_controller`] — orchestrator-facing gRPC.

pub mod agent_controller;
pub mod buffer;
pub mod config;
pub mod error;
pub mod grpc_server;
pub mod http_server;
pub mod orr_client;
pub mod qkc_client;
pub mod qrng_adapter;
pub mod scheduler;
pub mod sdn_client;
pub mod service;
pub mod token_bucket;
