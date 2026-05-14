use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrrConfig {
    pub node_id: String,
    pub grpc_addr: String,

    /// gRPC URL of the SDN.
    pub sdn_url: String,

    /// Peer ORR endpoints (NodeId -> gRPC URL).
    #[serde(default)]
    pub peers: HashMap<String, String>,

    /// Default PQC suite for new circuits.
    #[serde(default = "default_suite")]
    pub default_pqc_suite: String,

    /// Circuit idle timeout in seconds.
    #[serde(default = "default_ttl")]
    pub circuit_idle_ttl_s: u32,

    #[serde(default = "default_metrics")]
    pub metrics_addr: String,
}

fn default_suite() -> String { "kyber1024+dilithium5".into() }
fn default_ttl() -> u32 { 300 }
fn default_metrics() -> String { "0.0.0.0:9101".into() }
