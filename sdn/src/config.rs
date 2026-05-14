use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdnConfig {
    pub node_id: String,

    /// gRPC control-plane bind.
    pub grpc_addr: String,

    /// HTTP admin API bind (web UI / orchestrator).
    pub http_addr: String,

    #[serde(default = "default_metrics")]
    pub metrics_addr: String,

    /// Initial topology JSON. If set, loaded at boot. Same format as the
    /// Python project's `config/topology.json`.
    #[serde(default)]
    pub topology_file: Option<String>,

    /// Default path policy if none is specified.
    #[serde(default = "default_policy")]
    pub default_policy: String,

    /// How often to recompute MCF in milliseconds.
    #[serde(default = "default_mcf_period")]
    pub mcf_period_ms: u64,

    /// Debounce window for topology push events (ms).
    #[serde(default = "default_debounce")]
    pub push_debounce_ms: u64,
}

fn default_metrics() -> String { "0.0.0.0:9102".into() }
fn default_policy() -> String { "min_cost_flow".into() }
fn default_mcf_period() -> u64 { 1000 }
fn default_debounce() -> u64 { 100 }
