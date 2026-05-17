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

    /// Folder containing per-entity JSON files (QKC/, ORR/, DKMS/, SAE/), as
    /// produced by the Python SDN. Loaded at boot when set.
    #[serde(default)]
    pub topology_dir: Option<String>,

    /// Default path policy if none is specified.
    #[serde(default = "default_policy")]
    pub default_policy: String,

    /// How often to recompute MCF in milliseconds.
    #[serde(default = "default_mcf_period")]
    pub mcf_period_ms: u64,

    /// Debounce window for topology push events (ms).
    #[serde(default = "default_debounce")]
    pub push_debounce_ms: u64,

    /// K used by the K-shortest-paths pre-computation in the MCF solver.
    #[serde(default = "default_k_paths")]
    pub mcf_k_paths: usize,
}

fn default_metrics() -> String {
    "0.0.0.0:9102".into()
}
fn default_policy() -> String {
    "min_cost_flow".into()
}
fn default_mcf_period() -> u64 {
    1000
}
fn default_debounce() -> u64 {
    100
}
fn default_k_paths() -> usize {
    3
}
