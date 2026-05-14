//! Configuración del ORR.
//!
//! Como en el resto de módulos, viene de `config/default.toml` con
//! overrides opcionales en `config/local.toml` y variables de entorno
//! con prefijo `ORR_` (ver `common::config::load_config`).
//!
//! Ejemplo:
//!
//! ```toml
//! orr_id          = "ORR_1"
//! qkc_id          = 1
//! qkc_local_addr  = "127.0.0.1:7100"   # local_listen del QKC co-localizado
//! grpc_addr       = "0.0.0.0:50052"    # API hacia el DKMS / clientes
//! sdn_url         = "http://127.0.0.1:50053"
//! metrics_addr    = "0.0.0.0:9101"
//! default_max_hops = 0                 # 0 = passthrough, 1 = PQC E2E, -1 = onion
//!
//! [peers]                              # orr_id -> qkc_id del peer
//! ORR_2 = 2
//! ORR_3 = 3
//! ```

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrrConfig {
    /// ID lógico del ORR (string, lo elige el operador / SDN).
    pub orr_id: String,

    /// ID numérico del QKC co-localizado (mismo `qkc_id` que su config).
    pub qkc_id: u32,

    /// `host:port` del listener TCP **local** del QKC co-localizado.
    /// Por ahí mandamos `FRAME_LOCAL_SEND` y recibimos
    /// `FRAME_LOCAL_DELIVER`.
    pub qkc_local_addr: String,

    /// Donde sirvo gRPC hacia el DKMS o cualquier cliente superior.
    pub grpc_addr: String,

    /// URL gRPC de la SDN (se consulta puntualmente para resolver paths
    /// cuando `max_hops != 0`).
    pub sdn_url: String,

    #[serde(default = "default_metrics")]
    pub metrics_addr: String,

    /// Modo por defecto cuando un `SendMessage` no especifica `max_hops`.
    #[serde(default)]
    pub default_max_hops: i32,

    /// Mapa `orr_id -> qkc_id`. Lo que en el ORR Python hacía el
    /// `PeerDirectory` cargando ficheros JSON por peer; aquí lo dejamos
    /// declarativo en el TOML. Se puede actualizar en runtime via SDN
    /// (TODO: hook para eso).
    #[serde(default)]
    pub peers: HashMap<String, u32>,

    /// Mapa `orr_id -> base64(public_key)`. Las claves públicas ML-KEM
    /// de los peers, codificadas en base64 estándar (con padding).
    /// Necesarias para construir capas onion (`max_hops != 0`). Si
    /// falta la pubkey de un peer, las llamadas onion-routed contra
    /// ese peer fallan con `OrrError::NotFound`. Se puede mutar en
    /// runtime vía `PeerRegistry::put_pubkey` (SDN/gRPC futuro).
    #[serde(default)]
    pub peer_pubkeys: HashMap<String, String>,

    /// Suite PQC por defecto para handshakes onion. Hoy es informativo
    /// — el backend PQC todavía no está cableado (ver `handshake.rs`).
    #[serde(default = "default_suite")]
    pub default_pqc_suite: String,

    /// Tamaño de cola para entregas locales hacia los suscriptores
    /// `StreamDeliveries`. Si se llena, los suscriptores lentos pierden
    /// mensajes (modo lossy intencional — el control plane no se debe
    /// frenar por un consumidor congestionado).
    #[serde(default = "default_deliver_queue")]
    pub deliver_queue_capacity: usize,
}

fn default_metrics() -> String {
    "0.0.0.0:9101".into()
}
fn default_suite() -> String {
    "kyber1024+dilithium5".into()
}
fn default_deliver_queue() -> usize {
    4096
}
