//! Configuración del DKMS.
//!
//! Carga vía `common::config::load_config("dkms")` con resolución
//! `config/default.toml` ← `config/local.toml` ← variables `DKMS_*`
//! (separador `__` para anidados, p. ej. `DKMS_LISTEN__SAE_ADDR`).
//!
//! El DKMS expone **dos planos HTTP separados** porque cada uno tiene su
//! propio CA cliente:
//!
//! * `sae_addr` — ETSI 014 hacia SAEs (mTLS con `tls.sae_client_ca`).
//! * `peer_addr` — ETSI 020 entre DKMSs vecinos (mTLS con `tls.peer_dkms_ca`).
//!
//! Mantenerlos en puertos distintos evita mezclar trust roots en un mismo
//! `ClientCertVerifier` y simplifica el firewalling/operativa.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DkmsConfig {
    /// Identificador estable de esta instancia en el grafo SDN.
    pub node_id: String,

    pub listen: ListenCfg,
    pub tls: TlsCfg,
    pub southbound: SouthboundCfg,

    /// Vecinos DKMS conocidos. Clave = `node_id` del peer.
    #[serde(default)]
    pub peers: HashMap<String, PeerCfg>,

    #[serde(default)]
    pub buffer: BufferCfg,

    #[serde(default)]
    pub pending: PendingCfg,

    #[serde(default)]
    pub sae: SaeCfg,

    #[serde(default)]
    pub sae_binding: SaeBindingCfg,

    /// SAE → DKMS node estático para local-dev sin SDN. Clave = SAE id,
    /// valor = node_id del DKMS donde reside. Vacío por defecto: cuando
    /// la SDN esté cableada, el `SaeBindingCache` la consultará en vez
    /// de mirar aquí.
    #[serde(default)]
    pub sae_bindings: HashMap<String, String>,

    #[serde(default)]
    pub request: RequestCfg,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenCfg {
    /// ETSI 014 (SAE-facing) HTTP bind.
    pub sae_addr: SocketAddr,
    /// ETSI 020 (DKMS↔DKMS) HTTP bind.
    pub peer_addr: SocketAddr,
    /// gRPC `DkmsControl` (orquestador) bind.
    pub grpc_addr: SocketAddr,
    /// Prometheus `/metrics` bind.
    pub metrics_addr: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsCfg {
    /// Cert servidor de este DKMS (cadena PEM, primero la hoja).
    pub cert_path: PathBuf,
    /// Clave privada del cert servidor (PEM).
    pub key_path: PathBuf,
    /// CA que firma los certs cliente de los SAEs (verifier de `sae_addr`).
    pub sae_client_ca: PathBuf,
    /// CA que firma los certs de otros DKMSs (verifier de `peer_addr` y
    /// trust root para las llamadas salientes hacia peers).
    pub peer_dkms_ca: PathBuf,
    /// CA para las llamadas gRPC al control plane (SDN, QKC). Si falta,
    /// se reutiliza [`Self::peer_dkms_ca`].
    #[serde(default)]
    pub control_plane_ca: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SouthboundCfg {
    pub sdn_endpoint: String,
    pub qkc_endpoint: String,
    /// gRPC endpoint del ORR co-localizado. Opcional: si está vacío o
    /// ausente, el DKMS no abre conexión con ORR y sigue funcionando
    /// con HTTP/2 ETSI 020. Cuando se cablea el nuevo transporte por
    /// ORR↔QKC se usa este endpoint.
    #[serde(default)]
    pub orr_endpoint: Option<String>,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_rpc_timeout_ms")]
    pub rpc_timeout_ms: u64,
    /// `max_hops` por defecto al mandar vía ORR cuando un peer no lo
    /// fija explícitamente. `1` = PQC E2E (default seguro: una capa
    /// onion entre origen y destino, intermedios solo ven xor_ct).
    #[serde(default = "default_max_hops")]
    pub default_max_hops: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerCfg {
    /// URL base HTTPS del peer (incluye esquema y puerto). Ej:
    /// `https://dkms-b.internal:8443`. Usada solo cuando
    /// `transport == "http"`.
    pub endpoint: String,
    /// SNI a usar en el TLS handshake (si difiere del host del endpoint).
    #[serde(default)]
    pub sni: Option<String>,
    /// ID lógico del ORR del peer DKMS (ej. `"orr_22"`). Requerido si
    /// `transport == "orr"`; ignorado si `"http"`.
    #[serde(default)]
    pub orr_id: Option<String>,
    /// Transporte para los envíos de claves a este peer. Default
    /// `http` para no romper despliegues existentes.
    #[serde(default)]
    pub transport: PeerTransport,
    /// Override de `max_hops` para envíos ORR a este peer. Si ausente,
    /// usa `southbound.default_max_hops`.
    #[serde(default)]
    pub max_hops: Option<i32>,
    /// Hint de `orr_path` para modos onion `>=2` o `-1`. CSV de
    /// orr_ids (sin el origen — el ORR filtra self automáticamente —
    /// terminando en el orr_id del peer destino). Necesario mientras
    /// la SDN no calcule paths en Rust.
    #[serde(default)]
    pub orr_path: Option<String>,
}

/// Selector de transporte por peer.
///
/// * `Http` — POST ETSI 020 sobre HTTP/2 + mTLS (`peer_client.rs`).
///   Comportamiento original; AEAD-wrap con clave de transporte del
///   pool QKC.
/// * `Orr` — gRPC al ORR co-localizado (`southbound::orr::OrrClient`)
///   con body = K raw y `header_dkms` en `app_header`. El ORR aplica
///   onion según `max_hops` y el QKC OTP-cifra por enlace. NO se hace
///   AEAD-wrap encima.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerTransport {
    #[default]
    Http,
    Orr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferCfg {
    /// Claves de transporte por peer DKMS antes de aplicar backpressure.
    pub capacity_per_peer: usize,
    /// Umbral por debajo del cual el feeder vuelve a pedir a QKC.
    pub refill_low_watermark: usize,
    /// Tamaño de lote al pedir a QKC.
    pub refill_batch: u32,
}

impl Default for BufferCfg {
    fn default() -> Self {
        Self {
            capacity_per_peer: 4096,
            refill_low_watermark: 1024,
            refill_batch: 256,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCfg {
    /// TTL por defecto de una clave de sesión entregada por ETSI 020 si el
    /// container no trae un `extension_mandatory.ttl_seconds`.
    pub default_ttl_secs: u64,
    /// Periodo del barrido de expiraciones.
    pub sweep_interval_secs: u64,
}

impl Default for PendingCfg {
    fn default() -> Self {
        Self {
            default_ttl_secs: 86_400,
            sweep_interval_secs: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaeCfg {
    pub default_rate_keys_per_sec: u64,
    pub default_burst_keys: u64,
    /// Bytes que cuesta 1 token (ETSI 014: tradicionalmente 32).
    pub token_unit_bytes: u32,
}

impl Default for SaeCfg {
    fn default() -> Self {
        Self {
            default_rate_keys_per_sec: 100,
            default_burst_keys: 400,
            token_unit_bytes: 32,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SaeBindingCfg {
    pub ttl_secs: u64,
    pub max_entries: u64,
}

impl Default for SaeBindingCfg {
    fn default() -> Self {
        Self {
            ttl_secs: 60,
            max_entries: 16_384,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestCfg {
    /// Timeout para POST `/kmapi/v1/ext_keys` a un peer DKMS.
    pub peer_send_timeout_ms: u64,
    /// Timeout para esperar el ACK ETSI 020 antes de marcar el destino
    /// como fallido y reembolsar tokens.
    pub ack_wait_timeout_ms: u64,
    /// Cota de peers a los que distribuir en paralelo en una sola petición
    /// ETSI 014. Por encima se trocea.
    pub max_concurrent_peers: usize,
}

impl Default for RequestCfg {
    fn default() -> Self {
        Self {
            peer_send_timeout_ms: 1_500,
            ack_wait_timeout_ms: 2_500,
            max_concurrent_peers: 32,
        }
    }
}

fn default_connect_timeout_ms() -> u64 {
    1_500
}
fn default_rpc_timeout_ms() -> u64 {
    3_000
}
fn default_max_hops() -> i32 {
    1
}
