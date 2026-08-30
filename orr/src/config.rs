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
    /// rotación). **Activado por defecto**: por ese gRPC viaja el material de
    /// transporte, y en claro lo lee cualquiera que toque el cable. Exige
    /// `tls` (la identidad de nodo, CA de red): sin ella el ORR no arranca, y
    /// lo dice. `grpc_tls = false` lo deja en claro, y es una decisión que hay
    /// que escribir: sólo vale si DKMS y ORR comparten máquina o red interna
    /// de confianza. Sea cual sea, en los DOS extremos y en todos los ORR a
    /// la vez: el DKMS diala `https://` (`southbound.orr_tls`) y los pares
    /// también, diga lo que diga la URL que reparte la SDN.
    #[serde(default = "default_true")]
    pub grpc_tls: bool,

    #[serde(default = "default_metrics")]
    pub metrics_addr: String,

    /// Modo por defecto cuando un `SendMessage` no especifica `max_hops`.
    /// `0` (passthrough): el ORR sólo transporta. Es el default porque desde
    /// 2026-08-28 el DKMS sella el material extremo a extremo por su cuenta
    /// (`dkms/src/e2e.rs`); `1`, `≥2` y `-1` añaden cebolla ORR↔ORR encima
    /// como privacidad de camino, y meten el bootstrap ORR↔ORR en el camino
    /// crítico. El DKMS manda siempre el suyo (`has_max_hops = true`), así
    /// que esto sólo aplica a otros clientes.
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

    /// **Heredado.** Semilla ML-DSA (32 B, base64) para firmar la pubkey
    /// ML-KEM que se anuncia en `GetPublicKey`, verificable con las
    /// `peer_verify_keys` que hubiera que repartir a mano. Desde 2026-08-30
    /// el anuncio se firma con la clave del **cert de nodo** (`[tls]`) y el
    /// peer lo verifica contra la CA de red; con `[tls]` presente y clave
    /// ML-DSA esta semilla se ignora.
    #[serde(default)]
    pub sign_secret_seed: Option<common::config::SecretString>,

    /// Mapa `orr_id -> base64(ML-DSA verifying key)` de los peers, para
    /// verificar la firma de su anuncio de pubkey. Si falta la de un peer, su
    /// firma no se puede verificar (en `strict` se rechaza; en `tofu` se avisa).
    #[serde(default)]
    pub peer_verify_keys: HashMap<String, String>,

    /// Ancla de confianza del bootstrap (docs/SECURITY.md §Fase 6). `tofu`
    /// (default): acepta la pubkey que el peer anuncia por `GetPublicKey`
    /// (trust-on-first-use), avisando si difiere de un pin en `peer_pubkeys`.
    /// `strict`: exige un anuncio **firmado** — con la clave del cert de nodo
    /// del peer (cadena hasta la CA de red y SAN `dkms://<orr_id>`, lo normal
    /// desde 2026-08-30) o, heredado, con una `peer_verify_keys` configurada.
    /// Como el ancla es el certificado, `strict` no necesita config por par y
    /// sobrevive a los reinicios de la identidad ML-KEM efímera.
    #[serde(default)]
    pub bootstrap_trust: BootstrapTrust,

    /// Tamaño de cola para entregas locales hacia los suscriptores
    /// `StreamDeliveries`. Si se llena, los suscriptores lentos pierden
    /// mensajes (modo lossy intencional — el control plane no se debe
    /// frenar por un consumidor congestionado).
    #[serde(default = "default_deliver_queue")]
    pub deliver_queue_capacity: usize,

    /// Periodo entre rotaciones del `master_secret` por peer, en
    /// milisegundos. Cada periodo el iniciador (lex-smaller `orr_id`)
    /// negocia una época nueva con una keypair ML-KEM efímera fresca
    /// (`rotation.rs`); el respondedor la instala al recibir el FIN. Default
    /// `3600000` (1 h), alineado con `pqc_rekey_secs` del QKC y
    /// `transport_e2e.rekey_secs` del DKMS.
    ///
    /// Es forward secrecy real desde 2026-08-30 (antes la rotación estaba
    /// desconectada y todo corría en la época 0): la esk del respondedor se
    /// zeroiza tras el decap, así que capturar la clave long-term después no
    /// descifra las épocas pasadas. Solo importa para los modos cebolla
    /// (`max_hops != 0`): con el default `0` el ORR es un relé y el material
    /// va sellado extremo a extremo por el DKMS.
    #[serde(default = "default_rotation_period_ms")]
    pub rotation_period_ms: u64,

    /// Cuántas épocas de `master_secret` mantener vivas por peer, en cada
    /// extremo. Tolera lo que esté en vuelo durante una rotación. Default
    /// `3`; se acota a ≥ 2 porque entre que el respondedor instala N y el
    /// iniciador recibe el `ok`, el iniciador sigue cifrando con N−1.
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

fn default_true() -> bool {
    true
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
    3_600_000
}
fn default_epoch_history_keep() -> usize {
    3
}
