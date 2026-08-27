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

    /// Default path policy if none is specified.
    #[serde(default = "default_policy")]
    pub default_policy: String,

    /// How often to recompute MCF in milliseconds.
    #[serde(default = "default_mcf_period")]
    pub mcf_period_ms: u64,

    /// Debounce window for topology push events (ms).
    #[serde(default = "default_debounce")]
    pub push_debounce_ms: u64,

    /// How long a self-registered module may stay silent before it is dropped
    /// from the topology. Its announce loop doubles as a heartbeat, so this
    /// must comfortably exceed the modules' `sdn_announce_secs` (30 by
    /// default) — three missed announcements is the intent.
    ///
    /// `0` disables expiry entirely.
    #[serde(default = "default_presence_ttl")]
    pub presence_ttl_secs: u64,

    /// Qué mecanismo produce las rates de `/rate`: `num` (precios α-fair,
    /// default), `maxmin` (waterfilling exacto) o `lp` (el MCMCF-λ clásico,
    /// como oráculo/estudio). La env `SDN_RATE_ALLOCATOR` lo pisa, igual que
    /// `SDN_SOLVER` pisa el backend del LP.
    #[serde(default = "default_rate_allocator")]
    pub rate_allocator: String,

    /// α de la utilidad α-fair del asignador `num`. 1.0 = proportional
    /// fairness (nadie puede quedar a cero); subirla acerca al max-min a
    /// costa de rigidez numérica — para el max-min exacto está `maxmin`.
    #[serde(default = "default_num_alpha")]
    pub num_alpha: f64,

    /// Paso del gradiente de precios del asignador `num`.
    #[serde(default = "default_num_gamma")]
    pub num_gamma: f64,

    /// Peso del llenado proactivo frente al drenaje en la demanda del
    /// asignador `num` (`w = δ + peso·R/T`).
    #[serde(default = "default_num_fill_weight")]
    pub num_fill_weight: f64,
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

/// 3× el `sdn_announce_secs` por defecto de los módulos (30 s): se toleran dos
/// anuncios perdidos antes de dar a un nodo por muerto.
fn default_presence_ttl() -> u64 {
    90
}
fn default_rate_allocator() -> String {
    "num".into()
}
fn default_num_alpha() -> f64 {
    1.0
}
fn default_num_gamma() -> f64 {
    0.2
}
fn default_num_fill_weight() -> f64 {
    0.1
}
