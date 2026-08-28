//! Configuración del ORR.
//!
//! Como en el resto de módulos, viene de `config/default.toml` con
//! overrides opcionales en `config/local.toml` y variables de entorno
//! con prefijo `ORR_` (ver `common::config::load_config`).
//!
//! **Case-insensitivity en `orr_id` y peers**: la crate `config` lowercase
//! silenciosamente las claves de HashMap al leer el TOML, así que el
//! servicio normaliza `orr_id`, `peers` y `peer_pubkeys` a lowercase al
//! arrancar. Escribe `ORR_1` u `orr_1` indistintamente; en runtime todos
//! los ids serán lowercase.
//!
//! Ejemplo:
//!
//! ```toml
//! orr_id          = "orr_1"
//! qkc_id          = 1
//! qkc_local_addr  = "127.0.0.1:7100"   # local_listen del QKC co-localizado
//! grpc_addr       = "0.0.0.0:50052"    # API hacia el DKMS / clientes
//! sdn_url         = "http://127.0.0.1:50053"
//! metrics_addr    = "0.0.0.0:9101"
//! default_max_hops = 0                 # 0 = passthrough, 1 = PQC E2E, -1 = onion
//! default_pqc_suite = "ml-kem-768"     # ml-kem-512 / 768 / 1024
//!
//! [peers]                              # orr_id -> qkc_id del peer
//! orr_2 = 2
//! orr_3 = 3
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

    /// HTTP admin de la SDN (p. ej. `http://10.0.0.100:19002`) al que me
    /// anuncio para que me incluya en su topología. Es un puerto distinto del
    /// de `sdn_url`, que es gRPC. Sin esto el ORR funciona igual, pero alguien
    /// tiene que darlo de alta a mano.
    #[serde(default)]
    pub sdn_http_url: Option<String>,

    /// IP con la que me anuncio. Necesaria porque `grpc_addr` suele bindear
    /// `0.0.0.0`, que no le sirve a la SDN para alcanzarme.
    #[serde(default)]
    pub advertise_ip: Option<String>,

    /// Cada cuánto reanuncio. Es también mi heartbeat.
    #[serde(default = "default_announce_secs")]
    pub sdn_announce_secs: u64,

    /// TLS de cliente para el anuncio al SDN (docs/SECURITY.md §Fase 3).
    /// Solo se usa si `sdn_http_url` es `https://`; en claro se ignora.
    /// Con `grpc_tls = true` es además la identidad del servidor gRPC.
    #[serde(default)]
    pub tls: Option<common::http::ControlTlsCfg>,

    /// mTLS en el gRPC de este ORR: lo que le habla su DKMS (`SendMessage`,
    /// `StreamDeliveries`) y lo que le hablan los ORR pares (bootstrap y
    /// rotación). Exige `tls`. Con `false` (default) va en claro, y por ese
    /// canal viaja el material de transporte SIN cifrar: es el enlace
    /// DKMS↔ORR que "tiene que estar en red interna". Si DKMS y ORR no
    /// comparten máquina o la red interna no es de fiar, ponlo a `true` — en
    /// los DOS extremos: el DKMS pasa a `orr_endpoint = https://…` y los pares
    /// dialan `https://`. Los certificados son los de nodo (CA de red).
    #[serde(default)]
    pub grpc_tls: bool,

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

    /// Mapa `orr_id -> URL gRPC del peer ORR`. Lo usa el bootstrap
    /// task al arrancar para pedir las pubkeys a los peers vía
    /// `OrrControl::GetPublicKey` con backoff. Permite arrancar la
    /// red sin pegar pubkeys en TOML (que se regeneran cada boot).
    /// Las entradas que aparezcan también en `peer_pubkeys` se
    /// saltan (TOML tiene prioridad).
    #[serde(default)]
    pub peer_grpc_addrs: HashMap<String, String>,

    /// Suite PQC por defecto para handshakes onion. Hoy es informativo
    /// — el backend PQC todavía no está cableado (ver `handshake.rs`).
    #[serde(default = "default_suite")]
    pub default_pqc_suite: String,

    /// Semilla ML-DSA (32 B, base64) de la **identidad de firma estable** de
    /// este ORR (docs/SECURITY.md §Fase 6 PQC). Con ella se firma la pubkey
    /// ML-KEM que se anuncia en `GetPublicKey`, para que el peer detecte un
    /// MITM aunque la identidad ML-KEM sea efímera. Solo config local; la clave
    /// pública correspondiente se reparte a los peers como su `peer_verify_keys`.
    #[serde(default)]
    pub sign_secret_seed: Option<String>,

    /// Mapa `orr_id -> base64(ML-DSA verifying key)` de los peers, para
    /// verificar la firma de su anuncio de pubkey. Si falta la de un peer, su
    /// firma no se puede verificar (en `strict` se rechaza; en `tofu` se avisa).
    #[serde(default)]
    pub peer_verify_keys: HashMap<String, String>,

    /// Ancla de confianza del bootstrap (docs/SECURITY.md §Fase 6). `tofu`
    /// (default): acepta la pubkey que el peer anuncia por `GetPublicKey`
    /// (trust-on-first-use), avisando si difiere de un pin en `peer_pubkeys`.
    /// `strict`: exige que la pubkey case un pin configurado; rechaza el
    /// fetch si no. Nota: `strict` es práctico solo con identidades ORR
    /// estables entre reinicios — hoy la identidad se regenera en cada boot
    /// (ver `service.rs`), así que strict requiere persistir la identidad
    /// (pendiente, decisión de diseño).
    #[serde(default)]
    pub bootstrap_trust: BootstrapTrust,

    /// Tamaño de cola para entregas locales hacia los suscriptores
    /// `StreamDeliveries`. Si se llena, los suscriptores lentos pierden
    /// mensajes (modo lossy intencional — el control plane no se debe
    /// frenar por un consumidor congestionado).
    #[serde(default = "default_deliver_queue")]
    pub deliver_queue_capacity: usize,

    /// Periodo entre rotaciones de `master_secret` por peer, en
    /// milisegundos. Cada `rotation_period_ms` el initiator
    /// (lex-smaller `orr_id`) dispara una nueva época con keypair
    /// ML-KEM-768 efímera fresca. Default `30000` (30 s).
    ///
    /// Forward secrecy boundary (audit H-3 / Option B): tras cada
    /// rotación, la esk del responder se zeroiza, así que capturar la
    /// long-term sk en el futuro no descifra tráfico de épocas
    /// pasadas. Bajar este número aumenta la resistencia a captura
    /// (más boundaries) a costa de más tráfico de control y más CPU
    /// en encap/decap. Subirlo lo contrario.
    #[serde(default = "default_rotation_period_ms")]
    pub rotation_period_ms: u64,

    /// Cuántas épocas pasadas de `master_secret` mantener vivas
    /// simultáneamente por peer. Tolera tráfico in-flight durante la
    /// rotación: frames que viajan con `epoch_id = N - 1` siguen
    /// descifrables mientras `N - 1 ≥ latest_epoch - keep + 1`.
    /// Default `3`. Las épocas más viejas se zeroizan en cada
    /// rotación exitosa vía `peers.drop_old_epochs(keep)`.
    #[serde(default = "default_epoch_history_keep")]
    pub epoch_history_keep: usize,
}

/// Ancla de confianza del bootstrap de pubkeys (§Fase 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BootstrapTrust {
    /// Trust-on-first-use: acepta la pubkey anunciada; avisa si difiere de un pin.
    #[default]
    Tofu,
    /// Exige que la pubkey case un pin de `peer_pubkeys`; rechaza si no.
    Strict,
}

fn default_announce_secs() -> u64 {
    30
}

fn default_metrics() -> String {
    "0.0.0.0:9101".into()
}
fn default_suite() -> String {
    // Debe ser uno de los nombres soportados por
    // `common::crypto::pqc::suite::{ML_KEM_512, ML_KEM_768, ML_KEM_1024}`.
    // ML-KEM-768 es el sweet-spot NIST: pubkey 1184 B, ct 1088 B, ss 32 B.
    common::crypto::pqc::suite::ML_KEM_768.into()
}
fn default_deliver_queue() -> usize {
    4096
}
fn default_rotation_period_ms() -> u64 {
    30_000
}
fn default_epoch_history_keep() -> usize {
    3
}
