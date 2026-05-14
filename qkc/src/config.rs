use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QkcConfig {
    pub node_id: String,

    /// gRPC control-plane bind (e.g. `0.0.0.0:50051`).
    pub grpc_addr: String,

    /// Binary TCP hot-path bind (e.g. `0.0.0.0:7001`).
    pub tcp_bind: String,

    /// Per-peer TCP endpoints reachable from this node.
    #[serde(default)]
    pub peer_tcp: HashMap<String, String>,

    /// gRPC URL of the SDN to push capacity reports to.
    pub sdn_url: String,

    /// gRPC URL of the local quditto instance that provides raw key material.
    pub quditto_url: String,

    /// Maximum number of keys held in the buffer per peer.
    #[serde(default = "default_buffer")]
    pub buffer_max_keys: u64,

    /// Default key size in bits.
    #[serde(default = "default_keysize")]
    pub key_size_bits: u32,

    /// Token bucket refill rate (keys/s) — overridden by SDN if connected.
    #[serde(default = "default_rate")]
    pub default_refill_rate: u64,

    /// Prometheus metrics bind.
    #[serde(default = "default_metrics")]
    pub metrics_addr: String,
}

fn default_buffer() -> u64 { 8192 }
fn default_keysize() -> u32 { 256 }
fn default_rate() -> u64 { 1000 }
fn default_metrics() -> String { "0.0.0.0:9100".into() }
