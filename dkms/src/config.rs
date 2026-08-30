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

use common::security::SecurityLevel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DkmsConfig {
    /// Identificador estable de esta instancia en el grafo SDN.
    pub node_id: String,

    /// IP con la que me anuncio a la SDN. Necesaria porque `listen.sae_addr`
    /// suele bindear `0.0.0.0`, que no le sirve a la SDN para alcanzarme.
    #[serde(default)]
    pub advertise_ip: Option<String>,

    /// Cada cuánto reanuncio a la SDN. Es también mi heartbeat.
    #[serde(default = "default_announce_secs")]
    pub sdn_announce_secs: u64,

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

    #[serde(default)]
    pub generator: GeneratorCfg,

    /// Capa extremo a extremo DKMS↔DKMS sobre el material de transporte.
    /// Ver [`crate::e2e`]. Siempre activa: no hay modo "en claro".
    #[serde(default)]
    pub transport_e2e: TransportE2eCfg,

    /// Nivel de seguridad por defecto cuando una petición no especifica uno
    /// (vía extensions ETSI 014) y el peer destino no tiene override. Default
    /// `qkd_prefer`: usar QKD si hay camino QKD, si no PQC. Ver
    /// [`common::security::SecurityLevel`].
    #[serde(default)]
    pub default_security_level: SecurityLevel,
}

impl DkmsConfig {
    /// Nivel de seguridad por defecto a aplicar para un peer destino: su
    /// override en [`PeerCfg`] si existe, si no el global del DKMS.
    pub fn security_level_for(&self, peer: &str) -> SecurityLevel {
        self.peers
            .get(peer)
            .and_then(|p| p.security_level)
            .unwrap_or(self.default_security_level)
    }
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

    /// Aceptar la identidad del cert cliente venida en el header
    /// `ssl-client-cert` (lo inyecta nginx-ingress cuando termina mTLS en
    /// el borde). El PEM del header **no** se verifica contra ninguna CA:
    /// solo es seguro si estos puertos son alcanzables *exclusivamente* a
    /// través de ese proxy, que ya validó el cert. Por defecto `false` —
    /// la identidad sale del cert verificado por rustls en la capa TLS
    /// local. Los despliegues con nginx-ingress lo ponen a `true`.
    #[serde(default)]
    pub trust_proxy_client_cert_header: bool,
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
    /// **Ya no se usa.** El DKMS no habla con el QKC: el material de
    /// transporte entra por el ORR. Se admite para que los TOML antiguos
    /// sigan parseando; si está, se avisa y se ignora.
    #[serde(default)]
    pub qkc_endpoint: Option<String>,

    /// HTTP admin de la SDN (p. ej. `http://10.0.0.100:19002`) al que me
    /// anuncio para que me incluya en su topología. Puerto distinto del de
    /// `sdn_endpoint`, que es gRPC. Sin esto el DKMS funciona igual, pero
    /// alguien tiene que darlo de alta a mano.
    #[serde(default)]
    pub sdn_http_url: Option<String>,

    /// Id del ORR del que cuelgo. La SDN lo usa para colocarme en el grafo:
    /// yo cuelgo de un ORR, y ese ORR de un QKC. `orr_endpoint` no vale para
    /// esto — es una dirección, no un id.
    #[serde(default)]
    pub orr_id: Option<String>,
    /// gRPC endpoint del ORR co-localizado. Opcional: si está vacío o
    /// ausente, el DKMS no abre conexión con ORR y sigue funcionando
    /// con HTTP/2 ETSI 020. Cuando se cablea el nuevo transporte por
    /// ORR↔QKC se usa este endpoint.
    #[serde(default)]
    pub orr_endpoint: Option<String>,
    /// TLS hacia el ORR. **`true` por defecto**: por ese gRPC viaja el material
    /// de transporte, así que va con mTLS —este DKMS presenta su certificado
    /// de nodo y verifica el del ORR con la CA de red— aunque `orr_endpoint`
    /// diga `http://`: el esquema se sube a `https://` al arrancar. `false`
    /// respeta el esquema escrito (en claro con `http://`), y sólo vale si
    /// DKMS y ORR comparten máquina o red interna de confianza; el ORR tiene
    /// que llevar entonces `grpc_tls = false`, o no se entienden.
    #[serde(default = "default_true")]
    pub orr_tls: bool,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_rpc_timeout_ms")]
    pub rpc_timeout_ms: u64,
    /// `max_hops` por defecto al mandar vía ORR cuando un peer no lo
    /// fija explícitamente. `0` = passthrough: el ORR sólo transporta,
    /// porque desde 2026-08-28 el material va sellado extremo a extremo por
    /// el propio DKMS ([`crate::e2e`]) y la cebolla del ORR ya no aporta
    /// confidencialidad que no tenga. `1`, `≥2` o `-1` añaden encima las
    /// capas onion del ORR (privacidad de camino), a coste de CPU y del
    /// bootstrap ORR↔ORR en el camino crítico.
    #[serde(default = "default_max_hops")]
    pub default_max_hops: i32,
}

/// Capa extremo a extremo DKMS↔DKMS ([`crate::e2e`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransportE2eCfg {
    /// Suite ML-KEM del acuerdo de clave por par (`ml-kem-512|768|1024`).
    #[serde(default = "default_e2e_suite")]
    pub suite: String,
    /// Cada cuánto rota la época con cada peer (lo dispara el lex-menor).
    #[serde(default = "default_e2e_rekey_secs")]
    pub rekey_secs: u64,
    /// Épocas que se guardan por peer para abrir lo que aún esté en vuelo.
    #[serde(default = "default_e2e_history")]
    pub epoch_history_keep: usize,
    /// Anchura de la ventana anti-replay por peer emisor, en contadores.
    #[serde(default = "default_e2e_window")]
    pub replay_window: u64,
}

impl Default for TransportE2eCfg {
    fn default() -> Self {
        Self {
            suite: default_e2e_suite(),
            rekey_secs: default_e2e_rekey_secs(),
            epoch_history_keep: default_e2e_history(),
            replay_window: default_e2e_window(),
        }
    }
}

fn default_e2e_suite() -> String {
    common::crypto::pqc::suite::ML_KEM_768.to_owned()
}
fn default_e2e_rekey_secs() -> u64 {
    3600
}
fn default_e2e_history() -> usize {
    4
}
fn default_e2e_window() -> u64 {
    common::crypto::frame_mac::DEFAULT_WINDOW
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

    /// Override del nivel de seguridad por defecto para peticiones cuyo
    /// destino es este peer. Si ausente, usa `DkmsConfig.default_security_level`.
    /// Una petición ETSI 014 con `security_level` en sus extensions tiene
    /// precedencia sobre este default. Ver [`DkmsConfig::security_level_for`].
    #[serde(default)]
    pub security_level: Option<SecurityLevel>,
}

/// Selector de transporte por peer.
///
/// * `Http` — POST ETSI 020 sobre HTTP/2 + mTLS (`peer_client.rs`).
///   Comportamiento original; AEAD-wrap con clave de transporte del
///   pool QKC.
/// * `Orr` — gRPC al ORR co-localizado (`southbound::orr::OrrClient`)
///   con body = K sellada extremo a extremo por [`crate::e2e`] (tag en
///   `header_dkms`) y `header_dkms` en `app_header`. El ORR aplica onion
///   según `max_hops` (0 por defecto: sólo transporta) y el QKC OTP-cifra
///   por enlace.
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

/// `#[serde(default)]` a nivel de struct: un `[sae]` parcial en el TOML (el
/// renderer emite solo `enforce_authorization`) completa el resto con
/// `Default`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SaeCfg {
    pub default_rate_keys_per_sec: u64,
    pub default_burst_keys: u64,
    /// Bytes que cuesta 1 token (ETSI 014: tradicionalmente 32).
    pub token_unit_bytes: u32,
    /// Ventana de observación para activar SAEs y eviction (segundos).
    /// SAEs sin peticiones en esta ventana pierden su bucket y dejan de
    /// contar en `active_sae_count`. Réplica del Python (default 60s).
    pub observation_window_secs: f64,
    /// Floor mínimo del bucket capacity. Evita que `live_occupancy=0`
    /// en warmup haga rate-limit instant al primer cliente.
    pub min_capacity_tokens: f64,

    /// Exigir que el SAE autenticado (por mTLS) sea uno de los que este
    /// DKMS declara servir (`sae_bindings` cuyo valor es mi `node_id`,
    /// la misma lista que anuncio a la SDN). Cierra el hueco de que
    /// cualquier cert válido pudiera pedir claves en nombre de cualquier
    /// SAE. Por defecto `true` (fail-closed). Ponlo a `false` solo en
    /// despliegues donde la pertenencia SAE→DKMS es puramente dinámica vía
    /// SDN y no se declara localmente.
    #[serde(default = "default_enforce_authorization")]
    pub enforce_authorization: bool,
}

fn default_enforce_authorization() -> bool {
    true
}

impl Default for SaeCfg {
    fn default() -> Self {
        Self {
            default_rate_keys_per_sec: 100,
            default_burst_keys: 400,
            token_unit_bytes: 32,
            observation_window_secs: 60.0,
            min_capacity_tokens: 1.0,
            enforce_authorization: true,
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

/// Configuración del Generator que rellena los buffers ENC/DEC compartidos
/// entre DKMSs a la tasa que dicte el SDN.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneratorCfg {
    /// Si false, el Generator no arranca (caso de despliegues legacy HTTP/2
    /// puros donde el ENC buffer se llena via el flow ETSI 020 incoming).
    pub enabled: bool,
    /// Tamaño del key material a generar por clave (bytes). Típico 32 B
    /// (256 bits) — coincide con `token_unit_bytes` de la SAE config.
    pub key_size_bytes: usize,
    /// Periodo del scheduler tick (ms). Más pequeño = burst más fino.
    pub tick_ms: u64,
    /// Periodo de polling de rates al SDN (ms). El SDN re-calcula MCF
    /// cada `mcf_period_ms` (5s típico), así que 5000-10000 está bien.
    pub rate_refresh_ms: u64,
    /// Periodo de reporte `POST /demand` al SDN (ms). Cada DKMS envía,
    /// por cada peer, una snapshot `(level, capacity, δ_k)` que el
    /// solver MCMCF-λ consume como demanda escalada por λ. La EWMA del
    /// drain ya suaviza picos, así que 1000 ms es un buen default: el
    /// SDN reactúa rápido sin saturar la red de control. Sólo se POSTea
    /// cuando el batch tiene al menos un peer (ver `run_demand_loop`).
    pub demand_refresh_ms: u64,
    /// Deadline máximo que una clave permanece en `ack_pending` antes
    /// de descartarse (ms). Debe cubrir el round-trip ORR+QKC+ACK socket.
    pub ack_timeout_ms: u64,
    /// Periodo de barrido del reaper de `ack_pending` (ms).
    pub ack_reaper_ms: u64,
    /// Dirección TCP en la que este DKMS escucha ACKs entrantes de peers.
    /// Los peers la reciben en el header `ack_endpoint` de cada mensaje
    /// DKMS-BUFFER. Si no se configura, el Generator usa `listen.peer_addr`
    /// con un puerto offset definido por `ack_socket_port_offset`.
    pub ack_socket_addr: Option<SocketAddr>,

    /// Transporte de los ACK **salientes**: `"socket"` (default, TCP plano
    /// heredado) o `"etsi020"` (POST mTLS a `/kmapi/v1/ext_keys/ack`).
    ///
    /// El socket acepta conexiones de cualquiera y se cree el `from` que le
    /// mandan, así que un ACK forjado saca entradas de `ack_pending` y
    /// descuadra el generador (docs/SECURITY.md §Fase 4). Con `etsi020` la
    /// identidad la pone el certificado de cliente.
    ///
    /// Sigue en `socket` por defecto: el receptor autenticado ya existía
    /// (`handle_ext_keys_ack`), pero migrar la salida y retirar el socket está
    /// pendiente de verificación en testbed. Hay que ponerlo en los DOS
    /// extremos y comprobar que `generator.state` sigue moviendo `acked`.
    #[serde(default = "default_ack_transport")]
    pub ack_transport: String,
    /// Override del valor textual que se ANUNCIA a peers en el header
    /// ``ack_endpoint``. Cuando ``ack_socket_addr`` binda en ``0.0.0.0``
    /// (despliegues K8s), su ``to_string()`` produce ``0.0.0.0:PORT`` que
    /// los peers no pueden enrutar. Aquí se puede meter el DNS Service del
    /// pod, p.ej. ``"dkms-42:5002"``. Si está ``None``, el comportamiento
    /// es el legacy (stringificar ``ack_socket_addr``).
    #[serde(default)]
    pub ack_advertised_endpoint: Option<String>,
    /// Cap de tokens consumibles por un peer por tick. Evita que un peer
    /// hot monopolice el dispatch.
    pub max_tokens_per_peer_per_tick: u32,

    /// Emisiones simultáneas de TODO el tick, sumando todos los peers.
    ///
    /// El tick recorría los peers de uno en uno esperando a que terminara el
    /// lote de cada uno, así que una vuelta duraba la suma de las latencias en
    /// vez de `tick_ms`: con 9 peers eso son ~2,6 vueltas/s en lugar de 10, y
    /// la tasa por peer cae como O(1/N) — peor cuanto más grande es la malla.
    /// Medido el 2026-08-24 en CESGA: 83,7 claves/s por peer contra las 320
    /// que el bucket permitía y las 139 que la SDN asignaba.
    ///
    /// Ahora las emisiones de todos los peers van a la vez, pero **acotadas**:
    /// soltarlas sin límite multiplicaría por el número de peers la presión
    /// sobre el ORR, y este camino no tiene contrapresión — con el techo de
    /// tokens subido ×10 el proceso murió por OOM (16,6 GB). Este número es
    /// esa contrapresión, explícita y ajustable.
    ///
    /// Default 128: 4× lo que había en la práctica (un solo lote de 32) y muy
    /// por debajo de lo que reventó.
    ///
    /// Medido después, misma receta en CESGA: 151,8 claves/s por par ordenado
    /// frente a 84,4, y 13 666 agregadas frente a 7 594 — un +80 %. El DKMS
    /// pasa a consumir todo lo que la SDN le asigna, y el cuello se traslada a
    /// la fibra: el keystore del QKC llega a `enc=0 dec=0` con cientos de
    /// miles de `misses` en los enlaces cargados. La integridad aguanta el
    /// nuevo ritmo (2 600 claves muestreadas idénticas, `recv_corrupt = 0`).
    pub max_emits_in_flight: usize,

    /// Cap superior del bucket (cuántos segundos de rate pueden acumularse
    /// si el peer no ha consumido). Default 2s.
    pub bucket_cap_seconds: f64,
    /// Floor de rate de relleno (keys/s por peer) usado cuando el SDN aún no
    /// ha asignado rate para ese peer — p.ej. antes del primer solve MCMCF-λ,
    /// que a N grande puede tardar minutos (el LP no escala). `0.0` (default)
    /// mantiene el comportamiento legacy (solo rellena con la rate del SDN);
    /// `>0` desacopla el llenado de buffers del optimizador de rates del SDN.
    #[serde(default)]
    pub default_fill_rate_keys_per_s: f64,
    /// Cap (techo) de rate de relleno (keys/s por peer). A diferencia del floor,
    /// este LIMITA la rate efectiva por encima de lo que asigne el SDN. Sirve
    /// para experimentos de saturación controlada: con el SDN sano (p.ej. ~55
    /// keys/s/buffer a N=20) la oferta de pads supera con creces λ y nunca se
    /// satura; fijando un cap por debajo de λ se reproduce la presión del token
    /// bucket de forma determinista e independiente de la topología. La rate
    /// efectiva es `min(max(sdn_rate, floor), cap)`. `0.0` (default) = sin cap.
    #[serde(default)]
    pub max_fill_rate_keys_per_s: f64,
}

impl Default for GeneratorCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            key_size_bytes: 32,
            tick_ms: 100,
            rate_refresh_ms: 1_000,
            demand_refresh_ms: 1_000,
            ack_timeout_ms: 30_000,
            ack_reaper_ms: 1_000,
            ack_socket_addr: None,
            ack_transport: default_ack_transport(),
            ack_advertised_endpoint: None,
            max_tokens_per_peer_per_tick: 32,
            max_emits_in_flight: 128,
            bucket_cap_seconds: 2.0,
            default_fill_rate_keys_per_s: 0.0,
            max_fill_rate_keys_per_s: 0.0,
        }
    }
}

fn default_announce_secs() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

fn default_connect_timeout_ms() -> u64 {
    1_500
}
fn default_rpc_timeout_ms() -> u64 {
    3_000
}
fn default_ack_transport() -> String {
    "socket".to_string()
}

fn default_max_hops() -> i32 {
    0
}
