use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DkmsConfig {
    pub node_id: String,

    /// Customer-facing ETSI HTTP bind.
    pub http_addr: String,

    /// Optional mTLS for the HTTP listener.
    #[serde(default)]
    pub tls: Option<TlsCfg>,

    /// Orchestrator gRPC bind.
    pub grpc_addr: String,

    #[serde(default = "default_metrics")]
    pub metrics_addr: String,

    /// Backend service URLs.
    pub sdn_url:  String,
    pub orr_url:  String,
    pub qkc_url:  String,

    /// Optional local QRNG/quditto for hot-path key delivery.
    #[serde(default)]
    pub qrng_url: Option<String>,

    /// Per-SAE rate limit defaults — overridden per registration.
    #[serde(default = "default_rate")]
    pub default_sae_rate_keys_per_sec: u64,

    /// Round-robin scheduler period (ms).
    #[serde(default = "default_rr_period")]
    pub scheduler_period_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsCfg {
    pub cert_path: String,
    pub key_path:  String,
    #[serde(default)]
    pub client_ca: Option<String>,
}

fn default_metrics() -> String { "0.0.0.0:9103".into() }
fn default_rate() -> u64 { 100 }
fn default_rr_period() -> u64 { 50 }
