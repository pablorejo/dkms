//! Configuración del QKC.
//!
//! Toda la config viene de un fichero TOML referenciado en la CLI con
//! `--config`. Estructura:
//!
//! ```toml
//! qkc_id       = 1
//! peer_listen  = "0.0.0.0:7001"
//! local_listen = "0.0.0.0:7100"
//! admin_http   = "0.0.0.0:7200"
//!
//! # Una entrada por enlace (vecino directo).
//! #
//! # Cada enlace es QKD (default) o PQC:
//! #   * QKD — pide claves al quditto compartido por ETSI 014. Requiere
//! #     `quditto_url`.
//! #   * PQC — sin quditto: los dos QKC hacen un handshake ML-KEM sobre el
//! #     canal TCP QKC↔QKC y derivan de él un flujo de claves. NO lleva
//! #     `quditto_url`; opcionalmente `pqc_suite` (default ml-kem-768).
//!
//! [[links]]
//! neighbor_id        = 2
//! neighbor_peer_addr = "127.0.0.1:7002"
//! link_type          = "qkd"                     # default si se omite
//! quditto_url        = "http://127.0.0.1:8081"   # el quditto compartido del enlace 1↔2
//! key_size_bits      = 256
//!
//! [[links]]
//! neighbor_id        = 3
//! neighbor_peer_addr = "127.0.0.1:7003"
//! link_type          = "pqc"                     # enlace PQC: sin quditto
//! pqc_suite          = "ml-kem-768"              # opcional
//! key_size_bits      = 256
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::QkcError;

/// Tipo de canal de un enlace QKC↔QKC.
///
/// * `Qkd` (default) — material OTP servido por el quditto compartido del
///   enlace (ETSI 014). Es el comportamiento histórico.
/// * `Pqc` — sin quditto: los dos QKC acuerdan un secreto vía ML-KEM y
///   derivan de él un flujo determinista de claves que imita al feed QKD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LinkType {
    #[default]
    Qkd,
    Pqc,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QkcConfig {
    pub qkc_id: u32,

    /// Donde escucho frames de **otros QKCs**.
    pub peer_listen: String,

    /// Donde escucho frames del **ORR co-localizado**.
    pub local_listen: String,

    /// Donde monto el HTTP admin (forwarding-table push, healthz).
    pub admin_http: String,

    /// HTTP admin de la SDN (p. ej. `http://10.0.0.100:19002`) al que me
    /// anuncio para que me incluya en su topología. Sin esto el QKC funciona
    /// igual, pero alguien tiene que darlo de alta a mano en la SDN.
    #[serde(default)]
    pub sdn_url: Option<String>,

    /// IP con la que me anuncio a la SDN. Necesaria porque `admin_http` suele
    /// ser `0.0.0.0:*`, que no le sirve a la SDN para alcanzarme. Si se omite,
    /// se usa la IP de origen que la SDN ve en la conexión.
    #[serde(default)]
    pub advertise_ip: Option<String>,

    /// Cada cuánto reanuncio a la SDN. Es también el heartbeat del que
    /// cuelga la expiración de nodos, así que no lo subas sin mirar el TTL.
    #[serde(default = "default_announce_secs")]
    pub sdn_announce_secs: u64,

    /// TLS de cliente para el anuncio al SDN (docs/SECURITY.md §Fase 3).
    /// Solo se usa si `sdn_url` es `https://`; en claro se ignora.
    #[serde(default)]
    pub tls: Option<common::http::ControlTlsCfg>,

    /// Semilla ML-DSA (32 B, base64) de la **identidad de firma** de este QKC,
    /// usada para firmar el handshake en los enlaces con `pqc_auth = sign`
    /// (§Fase 5 upgrade). La clave pública correspondiente se reparte a los
    /// vecinos como su `peer_verify_key`. Solo config local. Sin ella, los
    /// enlaces en modo `sign` no pueden emitir handshakes firmados.
    #[serde(default)]
    pub sign_secret_seed: Option<common::config::SecretString>,

    /// Un entry por enlace QKC↔QKC con este vecino directo.
    #[serde(default)]
    pub links: Vec<LinkConfig>,
}

fn default_announce_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Deserialize)]
pub struct LinkConfig {
    /// ID del QKC vecino al otro lado del enlace.
    pub neighbor_id: u32,

    /// `host:port` del listener TCP-peer de ese QKC: los QKD relayean frames
    /// por él y los PQC además hacen el handshake ML-KEM por ese mismo canal.
    ///
    /// **Opcional en un enlace PQC cuando hay `sdn_url`.** La dirección no
    /// viaja en el anuncio —la SDN ya la sabe, porque cada QKC anuncia la
    /// suya— y vuelve en la lista de peers de la respuesta. Declarar solo
    /// `neighbor_id` deja que diga la SDN dónde está el vecino, que es lo
    /// único que ella conoce de todos; el enlace no se monta hasta que
    /// conteste, así que sin SDN no hay enlace (ver [`QkcConfig::validate`]).
    ///
    /// Un enlace QKD sí la exige: la SDN no crea enlaces QKD —necesitan un
    /// `quditto_url` que no puede inventar— así que nadie la rellenaría.
    #[serde(default)]
    pub neighbor_peer_addr: String,

    /// Tipo de canal: `qkd` (default) o `pqc`. Ver [`LinkType`].
    #[serde(default)]
    pub link_type: LinkType,

    /// URL base del quditto compartido por los dos QKCs (este lado y
    /// el vecino apuntan al mismo). Sirve ETSI 014 en
    /// `/api/v1/keys/{sae_id}/{enc,dec}_keys`. **Solo para enlaces QKD**
    /// (obligatorio); los PQC no lo llevan.
    #[serde(default)]
    pub quditto_url: Option<String>,

    /// SAE con el que este QKC figura en el KME, es decir el identificador que
    /// va en la ruta `/api/v1/keys/{sae_id}/…`.
    ///
    /// Es el **único** dato del KME que hay que declarar: el tamaño de clave y
    /// el máximo de claves por petición se descubren del `/status`
    /// ([`crate::kme::KmeClient::probe`]). Y no se puede descubrir porque el
    /// equipo no responde a nada sin él —un KME de ID Quantique devuelve
    /// `not managed SAE` incluso al `/status` si el id no es uno de los
    /// aprovisionados—, ni existe endpoint de catálogo en ETSI-014.
    ///
    /// Sin declarar se usa el `qkc_id`, que es lo que espera quditto (ignora el
    /// id y sirve del mismo pool). Contra hardware real hay que ponerlo: es el
    /// SAE **destino** del par, el que el operador del KME aprovisionó.
    #[serde(default)]
    pub sae_id: Option<String>,

    // ---- identidad hacia el KME (solo enlaces QKD — A6, docs/SECURITY.md).
    //
    // Cada KME es PRIVADO y tiene su propia PKI (del fabricante o de la
    // institución que lo opera): el QKC se autentica ante ESE KME con una
    // credencial de SU autoridad, no con el cert de red. Un QKC lleva por
    // tanto 1 cert de red + 1 credencial por KME conectado. Los tres campos
    // van juntos —o se declaran los tres, o ninguno—; sin declarar se cae a
    // la identidad de red (`[tls]`), que es la simplificación de la prueba
    // con quditto (acepta la net-ca). En producción son autoridades
    // separadas.
    /// Cert de cliente para ESTE KME (emitido por su PKI privada).
    #[serde(default)]
    pub kme_cert: Option<std::path::PathBuf>,

    /// Clave privada del `kme_cert`.
    #[serde(default)]
    pub kme_key: Option<std::path::PathBuf>,

    /// CA de ESE KME: verifica su cert de servidor en el mTLS ETSI 014.
    #[serde(default)]
    pub kme_ca: Option<std::path::PathBuf>,

    /// Parameter set ML-KEM para el handshake del enlace. En PQC es la fuente
    /// de claves; en QKD (A4) el handshake establece solo la raíz del sello
    /// por-frame. Default: `ml-kem-768`. Ver [`common::crypto::pqc::suite`].
    #[serde(default = "default_pqc_suite")]
    pub pqc_suite: String,

    /// Tamaño de la clave OTP en bits. Default: 1024 (mejor amortización
    /// del HTTP a quditto que con 256, ya que 1 clave cubre 128 B de
    /// payload). En enlaces PQC fija la longitud del material derivado por
    /// clave; debe ser idéntico en ambos extremos.
    #[serde(default = "default_key_size")]
    pub key_size_bits: u32,

    /// **Re-keying del handshake**. Rota el secreto ML-KEM cada
    /// `pqc_rekey_keys` claves emitidas. `0` = sin disparo por volumen.
    /// Default 1000. Acota el blast-radius por secreto. En enlaces QKD el
    /// disparo por volumen queda inerte (nadie consume claves del store: solo
    /// se deriva la raíz del sello) — allí manda `pqc_rekey_secs`.
    #[serde(default = "default_rekey_keys")]
    pub pqc_rekey_keys: u64,

    /// **Re-keying del handshake**: rota el secreto cada `pqc_rekey_secs`
    /// segundos (tope de edad, p. ej. enlaces de poco tráfico; en QKD, la
    /// cadencia de la raíz del sello). `0` = sin disparo por tiempo. Default
    /// 3600 (1 h). La rotación ocurre en `max(pqc_rekey_keys,
    /// pqc_rekey_secs)` (lo que llegue primero). `0 && 0` ⇒ secreto único
    /// (comportamiento histórico, sin forward secrecy).
    #[serde(default = "default_rekey_secs")]
    pub pqc_rekey_secs: u64,

    /// **Re-keying PQC**: épocas pre-cargadas por delante de la activa (la
    /// rotación es transparente, sin latencia). Default 2. Se fuerza a 0 si el
    /// re-keying está desactivado.
    #[serde(default = "default_rekey_lookahead")]
    pub pqc_rekey_lookahead: u32,

    // ---- modelo físico del enlace: el QKC no lo usa, se lo anuncia a la SDN.
    //
    // La SDN dimensiona la arista con `cap = r0 · 10^(−alpha·d/10)`, y quien
    // conoce esos valores es la institución: con hardware QKD son su fibra, y
    // con un quditto son los que le configuró. En enlaces PQC el modelo es
    // `capacity_keys_per_s` (default de la SDN: 10 000) y r0/alpha/distance
    // no se aplican.
    /// `R₀` del enlace en claves/s a distancia 0.
    #[serde(default)]
    pub r0: Option<f64>,

    /// `α`, atenuación de la fibra en dB/km.
    #[serde(default)]
    pub alpha: Option<f64>,

    /// Longitud del enlace en km.
    #[serde(default)]
    pub distance_km: Option<u32>,

    /// Capacidad declarada de un enlace PQC en claves/s (ignorada en QKD).
    /// Como r0/alpha/distance, el QKC no la usa: viaja a la SDN, que
    /// dimensiona la arista con ella. Sin declarar, aplica el default de la
    /// SDN.
    #[serde(default)]
    pub capacity_keys_per_s: Option<f64>,

    /// **Secreto pre-compartido por enlace** (base64) para autenticar el
    /// handshake PQC (docs/SECURITY.md §Fase 5). Solo config local: el SDN no
    /// transporta secretos. Sin él, el handshake va sin autenticar (frames
    /// 0x21/0x22), como hasta ahora. Debe ser idéntico en ambos extremos.
    #[serde(default)]
    pub link_psk: Option<common::config::SecretString>,

    /// Política de autenticación del handshake PQC de este enlace. Ver `PqcAuth`
    /// (`off` | `prefer` | `require` = HMAC-PSK; `sign` = firma ML-DSA).
    ///
    /// **Ausente ⇒ default según haya identidad de nodo**: con `[tls]`, el
    /// handshake del enlace —PQC o QKD (A4)— va **firmado con el cert** por
    /// defecto (`sign`, patrón `grpc_tls`: seguro por defecto). Un valor
    /// explícito manda —incluido `off` para desactivarlo—. Lee el efectivo con
    /// [`LinkConfig::effective_pqc_auth`], no este campo.
    #[serde(default)]
    pub pqc_auth: Option<PqcAuth>,

    /// Clave pública ML-DSA del **peer** (base64) para verificar su firma del
    /// handshake cuando `pqc_auth = sign`. Solo config local. Ver §Fase 5.
    /// Camino **legacy**: con identidad de cert la verificación es contra la
    /// net-CA + SAN, sin esta clave por-par.
    #[serde(default)]
    pub peer_verify_key: Option<String>,

    /// Política de autenticación de los **frames de datos** de este enlace.
    /// Ver [`FrameAuth`]. El handshake da autenticación de *entidad*; esto da
    /// integridad, autenticación de *origen de datos* y frescura de cada frame.
    ///
    /// **Ausente ⇒ default según haya identidad de nodo**, igual que
    /// [`pqc_auth`](Self::pqc_auth): con `[tls]`, `require` por defecto en los
    /// DOS tipos de enlace. La raíz del sello es el secreto de la ÉPOCA del
    /// handshake del enlace (no una PSK), así que no hay nada que repartir.
    /// Lee el efectivo con [`LinkConfig::effective_frame_auth`].
    #[serde(default)]
    pub frame_auth: Option<FrameAuth>,
}

/// Política de autenticación de los frames de datos (`FRAME_RECV`/`FRAME_RELAY`).
///
/// El payload va cifrado con OTP, que es maleable: sin MAC, un atacante en el
/// cable puede modificar el ciphertext y reinyectar frames viejos sin que nada
/// lo note. Con MAC (`common::crypto::frame_mac`) cada frame lleva
/// `session ‖ counter ‖ tag` y el receptor mantiene una ventana anti-replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FrameAuth {
    /// Sin autenticar (comportamiento histórico). Frames 0x01/0x02.
    #[default]
    Off,
    /// Firma los frames salientes si hay `link_psk`, y acepta tanto los
    /// autenticados (0x04/0x05) como los que llegan en claro. Es el escalón de
    /// migración: despliega en `prefer`, reparte PSKs, sube a `require`.
    Prefer,
    /// Exige frames autenticados: los que lleguen en claro se descartan. Sin
    /// `link_psk` el arranque falla, en vez de correr sin autenticar creyendo
    /// que sí.
    Require,
}

impl FrameAuth {
    /// `true` si hay que poner MAC a los frames salientes.
    pub fn signs(&self) -> bool {
        matches!(self, FrameAuth::Prefer | FrameAuth::Require)
    }
    /// `true` si hay que descartar los frames que lleguen sin MAC.
    pub fn rejects_plaintext(&self) -> bool {
        matches!(self, FrameAuth::Require)
    }
}

/// Política de autenticación del handshake PQC por enlace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PqcAuth {
    /// Sin autenticar (comportamiento histórico). Frames 0x21/0x22.
    #[default]
    Off,
    /// HMAC-PSK: autentica si hay `link_psk`; acepta frames autenticados y en
    /// claro. Para migración: despliega en `prefer`, reparte PSKs, sube a
    /// `require`.
    Prefer,
    /// HMAC-PSK: exige handshake autenticado; descarta INIT/RESP sin MAC válido.
    Require,
    /// **Firma post-cuántica ML-DSA** (docs/SECURITY.md §Fase 5 upgrade): exige
    /// que el handshake vaya firmado (frames 0x26/0x27) y verifica con la
    /// clave pública del peer (`peer_verify_key`). Necesita también el seed de
    /// firma de este nodo (`sign_secret_seed`). A diferencia del PSK simétrico,
    /// solo se comparten claves **públicas**.
    Sign,
}

fn default_key_size() -> u32 {
    1024
}

pub(crate) fn default_pqc_suite() -> String {
    common::crypto::pqc::suite::ML_KEM_768.to_string()
}

pub(crate) fn default_rekey_keys() -> u64 {
    1000
}

pub(crate) fn default_rekey_secs() -> u64 {
    3600
}

pub(crate) fn default_rekey_lookahead() -> u32 {
    2
}

impl LinkConfig {
    /// Lookahead efectivo: 0 si el re-keying está desactivado (`keys==0 &&
    /// secs==0`), si no `pqc_rekey_lookahead`. Así un enlace sin rotación usa
    /// una sola época sin pre-cargar épocas inútiles.
    pub fn effective_lookahead(&self) -> u32 {
        if self.pqc_rekey_keys == 0 && self.pqc_rekey_secs == 0 {
            0
        } else {
            self.pqc_rekey_lookahead
        }
    }

    /// `pqc_auth` efectivo. Con identidad de nodo (`[tls]` cargado), el
    /// handshake del enlace va firmado con el cert por defecto (`sign`): la
    /// verificación es contra la net-CA + SAN, sin material por-par, así que
    /// hasta los enlaces que crea la SDN quedan autenticados sin tocar nada.
    /// Aplica a los DOS tipos de enlace: en PQC el handshake es además la
    /// fuente de claves; en QKD (A4, opción B) establece solo la raíz del
    /// sello por-frame — las claves de datos siguen viniendo del KME, que
    /// sigue siendo la raíz de confianza del MATERIAL. Un valor explícito en
    /// el TOML manda siempre —incluido `off`—.
    pub fn effective_pqc_auth(&self, has_node_identity: bool) -> PqcAuth {
        match self.pqc_auth {
            Some(v) => v,
            None if has_node_identity => PqcAuth::Sign,
            None => PqcAuth::Off,
        }
    }

    /// `frame_auth` efectivo, con el mismo criterio que
    /// [`effective_pqc_auth`](Self::effective_pqc_auth): con identidad de
    /// nodo, el sello por-frame va en `require` por defecto. Su raíz es el
    /// secreto de la ÉPOCA del handshake del enlace, disponible en ambos
    /// extremos en cuanto éste —ya autenticado por el cert— establece la
    /// primera. En PQC no fluye ningún frame antes de la primera época (el
    /// handshake ES la fuente de claves); en QKD la ventana entre el arranque
    /// y la primera época es de milisegundos (el ML-KEM sobre el TCP ya
    /// abierto gana al primer relleno HTTP del KME) y se autocura: un NOTIFY
    /// descartado deja huérfanas unas claves que el flujo repone. Un valor
    /// explícito manda.
    pub fn effective_frame_auth(&self, has_node_identity: bool) -> FrameAuth {
        match self.frame_auth {
            Some(v) => v,
            None if has_node_identity => FrameAuth::Require,
            None => FrameAuth::Off,
        }
    }

    /// Identidad de cliente hacia el KME de ESTE enlace: la credencial de la
    /// PKI privada del KME si está declarada (`kme_cert`/`kme_key`/`kme_ca`),
    /// si no la identidad de red del nodo (`[tls]` — la prueba con quditto).
    /// `None` = KME en claro (el histórico intra-institución). Las mezclas
    /// parciales las rechaza [`QkcConfig::validate`], no este método.
    pub fn kme_client_tls<'a>(
        &'a self,
        node_tls: Option<&'a common::http::ControlTlsCfg>,
    ) -> Option<common::http::ClientTls<'a>> {
        match (&self.kme_cert, &self.kme_key, &self.kme_ca) {
            (Some(cert), Some(key), Some(ca)) => Some(common::http::ClientTls {
                ca_path: ca,
                cert_path: cert,
                key_path: key,
            }),
            _ => node_tls.map(|t| t.as_client_tls()),
        }
    }
}

impl QkcConfig {
    /// Carga desde fichero TOML.
    pub fn load(path: &Path) -> Result<Self, QkcError> {
        let raw = std::fs::read_to_string(path)?;
        let cfg: QkcConfig = toml::from_str(&raw)
            .map_err(|e| QkcError::BadRequest(format!("config TOML inválido: {e}")))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), QkcError> {
        for link in &self.links {
            if link.neighbor_id == self.qkc_id {
                return Err(QkcError::BadRequest(format!(
                    "link.neighbor_id == qkc_id ({})",
                    self.qkc_id
                )));
            }
            if link.key_size_bits == 0 || link.key_size_bits % 8 != 0 {
                return Err(QkcError::BadRequest(format!(
                    "link.key_size_bits must be a positive multiple of 8, got {}",
                    link.key_size_bits
                )));
            }
            match link.link_type {
                LinkType::Qkd => {
                    // La SDN solo crea enlaces PQC, así que aquí no hay quien
                    // rellene la dirección después.
                    if link.neighbor_peer_addr.is_empty() {
                        return Err(QkcError::BadRequest(format!(
                            "QKD link to {} requires neighbor_peer_addr: the SDN only creates PQC \
                             links, so nothing would fill it in",
                            link.neighbor_id
                        )));
                    }
                    if link.quditto_url.is_none() {
                        return Err(QkcError::BadRequest(format!(
                            "QKD link to {} requires quditto_url",
                            link.neighbor_id
                        )));
                    }
                    // Credencial de KME: o los tres campos o ninguno. Una
                    // mezcla parcial es casi seguro un typo, y caer en
                    // silencio al cert de red daría mTLS con la autoridad
                    // equivocada creyendo que se usa la del KME.
                    let kme_set = [
                        link.kme_cert.is_some(),
                        link.kme_key.is_some(),
                        link.kme_ca.is_some(),
                    ]
                    .iter()
                    .filter(|b| **b)
                    .count();
                    if kme_set != 0 && kme_set != 3 {
                        return Err(QkcError::BadRequest(format!(
                            "QKD link to {}: kme_cert/kme_key/kme_ca van juntos (o los tres o \
                             ninguno); hay {kme_set} de 3",
                            link.neighbor_id
                        )));
                    }
                }
                LinkType::Pqc => {
                    // Sin dirección, el único que puede decir dónde está el
                    // vecino es la SDN. Sin ella el enlace no se montaría
                    // nunca y el síntoma sería un QKC sano que no cifra con
                    // nadie; mejor no arrancar.
                    if link.neighbor_peer_addr.is_empty() && self.sdn_url.is_none() {
                        return Err(QkcError::BadRequest(format!(
                            "PQC link to {} has no neighbor_peer_addr and there is no sdn_url to \
                             learn it from: declare one of the two",
                            link.neighbor_id
                        )));
                    }
                    if link.quditto_url.is_some() {
                        return Err(QkcError::BadRequest(format!(
                            "PQC link to {} must not set quditto_url",
                            link.neighbor_id
                        )));
                    }
                    if link.kme_cert.is_some() || link.kme_key.is_some() || link.kme_ca.is_some() {
                        return Err(QkcError::BadRequest(format!(
                            "PQC link to {} must not set kme_cert/kme_key/kme_ca (no KME here)",
                            link.neighbor_id
                        )));
                    }
                    // Rechaza un suite desconocido en carga, no en el handshake.
                    common::crypto::pqc::kem_for(&link.pqc_suite).map_err(|e| {
                        QkcError::BadRequest(format!(
                            "PQC link to {}: invalid pqc_suite {:?}: {e}",
                            link.neighbor_id, link.pqc_suite
                        ))
                    })?;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "qkc_id = 1\n\
        peer_listen = \"0.0.0.0:7001\"\n\
        local_listen = \"0.0.0.0:7100\"\n\
        admin_http = \"0.0.0.0:7200\"\n";

    fn parse(toml_str: &str) -> QkcConfig {
        toml::from_str(toml_str).expect("valid TOML")
    }

    /// El QKC hace `info!(?cfg, "qkc starting")` al arrancar y `docker logs`
    /// es el canal de diagnóstico documentado: ni la PSK de un enlace ni la
    /// semilla de firma pueden salir por ahí.
    #[test]
    fn debug_output_never_contains_secrets() {
        let cfg = parse(&format!(
            "{BASE}sign_secret_seed = \"SEMILLA-SECRETA\"\n\
             sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\n\
             link_psk = \"PSK-SECRETA\"\n"
        ));
        assert_eq!(cfg.links[0].link_psk.as_deref(), Some("PSK-SECRETA"));
        assert_eq!(cfg.sign_secret_seed.as_deref(), Some("SEMILLA-SECRETA"));
        let dbg = format!("{cfg:?}");
        assert!(!dbg.contains("PSK-SECRETA"), "link_psk en el Debug: {dbg}");
        assert!(
            !dbg.contains("SEMILLA-SECRETA"),
            "sign_secret_seed en el Debug: {dbg}"
        );
    }

    /// Un vecino se puede declarar solo por id: la dirección la pone la SDN,
    /// que es la única que la conoce de todos los QKC. Lo que el QKC sabe de
    /// su topología es con QUIÉN tiene fibra, no dónde está el otro.
    #[test]
    fn a_pqc_neighbour_may_be_declared_by_id_alone() {
        let cfg = parse(&format!(
            "{BASE}sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\n"
        ));
        assert_eq!(cfg.links[0].neighbor_id, 2);
        assert!(
            cfg.links[0].neighbor_peer_addr.is_empty(),
            "sin dirección hasta que conteste la SDN"
        );
        cfg.validate().expect("con sdn_url es válido");
    }

    /// ...pero sin SDN no hay quien la rellene, y el enlace no se montaría
    /// nunca: un QKC aparentemente sano que no cifra con nadie. Mejor no
    /// arrancar que dejarlo así.
    #[test]
    fn a_neighbour_by_id_alone_needs_an_sdn_to_resolve_it() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\n"
        ));
        let err = cfg
            .validate()
            .expect_err("sin sdn_url no se puede resolver");
        assert!(
            format!("{err}").contains("no sdn_url"),
            "el error debe decir qué falta, no solo que falla: {err}"
        );
    }

    /// La SDN no crea enlaces QKD —necesitan un `quditto_url` que no puede
    /// inventar—, así que ahí la dirección sigue siendo obligatoria.
    #[test]
    fn a_qkd_neighbour_still_needs_its_address() {
        let cfg = parse(&format!(
            "{BASE}sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"qkd\"\n\
             quditto_url = \"http://kme:20010\"\n"
        ));
        let err = cfg.validate().expect_err("un QKD sin dirección no vale");
        assert!(format!("{err}").contains("neighbor_peer_addr"), "{err}");
    }

    #[test]
    fn link_without_type_defaults_to_qkd_and_needs_quditto() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             quditto_url = \"http://127.0.0.1:8081\"\nkey_size_bits = 256\n"
        ));
        assert_eq!(cfg.links[0].link_type, LinkType::Qkd);
        cfg.validate().expect("QKD link with quditto_url is valid");
    }

    #[test]
    fn qkd_link_without_quditto_url_is_rejected() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             key_size_bits = 256\n"
        ));
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pqc_link_parses_without_quditto_and_validates() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"pqc\"\nkey_size_bits = 256\n"
        ));
        assert_eq!(cfg.links[0].link_type, LinkType::Pqc);
        assert!(cfg.links[0].quditto_url.is_none());
        // suite por defecto.
        assert_eq!(
            cfg.links[0].pqc_suite,
            common::crypto::pqc::suite::ML_KEM_768
        );
        cfg.validate()
            .expect("PQC link without quditto_url is valid");
    }

    #[test]
    fn sdn_announce_fields_are_optional_and_default_sanely() {
        // Sin bloque de SDN: el QKC funciona igual, solo que nadie lo da de
        // alta solo. No debe fallar el parseo.
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"pqc\"\nkey_size_bits = 256\n"
        ));
        assert!(cfg.sdn_url.is_none());
        assert!(cfg.advertise_ip.is_none());
        assert_eq!(cfg.sdn_announce_secs, 30);
        assert!(cfg.links[0].r0.is_none());
    }

    #[test]
    fn sdn_announce_fields_and_link_model_parse() {
        let cfg = parse(
            "qkc_id = 1\npeer_listen = \"0.0.0.0:7001\"\nlocal_listen = \"0.0.0.0:7100\"\n\
             admin_http = \"0.0.0.0:7200\"\nsdn_url = \"http://10.0.0.100:19002\"\n\
             advertise_ip = \"10.0.0.11\"\nsdn_announce_secs = 15\n\
             [[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"qkd\"\nquditto_url = \"http://127.0.0.1:8081\"\n\
             key_size_bits = 256\nr0 = 2000.0\nalpha = 0.2\ndistance_km = 5\n",
        );
        assert_eq!(cfg.sdn_url.as_deref(), Some("http://10.0.0.100:19002"));
        assert_eq!(cfg.advertise_ip.as_deref(), Some("10.0.0.11"));
        assert_eq!(cfg.sdn_announce_secs, 15);
        assert_eq!(cfg.links[0].r0, Some(2000.0));
        assert_eq!(cfg.links[0].alpha, Some(0.2));
        assert_eq!(cfg.links[0].distance_km, Some(5));
        cfg.validate().expect("link model fields are informational");
    }

    #[test]
    fn pqc_link_with_quditto_url_is_rejected() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"pqc\"\nquditto_url = \"http://127.0.0.1:8081\"\nkey_size_bits = 256\n"
        ));
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn pqc_link_with_unknown_suite_is_rejected() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"pqc\"\npqc_suite = \"kyber-classic\"\nkey_size_bits = 256\n"
        ));
        assert!(cfg.validate().is_err());
    }

    /// A3: en un enlace PQC sin `pqc_auth`/`frame_auth` explícitos, la
    /// identidad de nodo (`[tls]`) los enciende por defecto — handshake firmado
    /// con el cert (`sign`) y sello por-frame `require` (su raíz es el secreto
    /// de la época, no una PSK). Sin identidad, todo sigue en `off`.
    #[test]
    fn pqc_link_auth_defaults_on_with_node_identity() {
        let cfg = parse(&format!(
            "{BASE}sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\n"
        ));
        let link = &cfg.links[0];
        assert!(link.pqc_auth.is_none(), "sin poner en el TOML");
        assert!(link.frame_auth.is_none());
        // Con identidad de nodo: seguro por defecto.
        assert_eq!(link.effective_pqc_auth(true), PqcAuth::Sign);
        assert_eq!(link.effective_frame_auth(true), FrameAuth::Require);
        // Sin identidad: no hay con qué, se queda en off (camino legacy).
        assert_eq!(link.effective_pqc_auth(false), PqcAuth::Off);
        assert_eq!(link.effective_frame_auth(false), FrameAuth::Off);
    }

    /// Un valor explícito en el TOML manda siempre, también para desactivar:
    /// `off` gana aunque haya identidad de nodo.
    #[test]
    fn explicit_link_auth_overrides_the_default() {
        let cfg = parse(&format!(
            "{BASE}sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\n\
             pqc_auth = \"off\"\nframe_auth = \"prefer\"\n"
        ));
        let link = &cfg.links[0];
        assert_eq!(link.pqc_auth, Some(PqcAuth::Off));
        assert_eq!(link.frame_auth, Some(FrameAuth::Prefer));
        assert_eq!(
            link.effective_pqc_auth(true),
            PqcAuth::Off,
            "off explícito manda"
        );
        assert_eq!(link.effective_frame_auth(true), FrameAuth::Prefer);
    }

    /// A6: la credencial hacia el KME es POR ENLACE y de la PKI privada de
    /// ESE KME (cada KME es una autoridad propia). Declarada completa se usa;
    /// sin declarar se cae a la identidad de red del nodo (la prueba con
    /// quditto); sin ninguna de las dos, en claro (histórico).
    #[test]
    fn kme_credentials_are_per_link_with_net_identity_fallback() {
        let qkd = |extra: &str| {
            parse(&format!(
                "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
                 link_type = \"qkd\"\nquditto_url = \"https://kme:20010\"\n{extra}"
            ))
        };
        let node_tls = common::http::ControlTlsCfg {
            cert_path: "/net/qkc-1.crt".into(),
            key_path: "/net/qkc-1.key".into(),
            control_plane_ca: "/net/net-ca.crt".into(),
        };

        let cfg = qkd(
            "kme_cert = \"/kme-a/client.crt\"\nkme_key = \"/kme-a/client.key\"\n\
                       kme_ca = \"/kme-a/ca.crt\"\n",
        );
        cfg.validate().expect("los tres campos juntos valen");
        let t = cfg.links[0].kme_client_tls(Some(&node_tls)).unwrap();
        assert_eq!(t.cert_path, std::path::Path::new("/kme-a/client.crt"));
        assert_eq!(t.ca_path, std::path::Path::new("/kme-a/ca.crt"));

        // Sin credencial de KME: la identidad de red (quditto acepta net-ca).
        let cfg = qkd("");
        let t = cfg.links[0].kme_client_tls(Some(&node_tls)).unwrap();
        assert_eq!(t.cert_path, std::path::Path::new("/net/qkc-1.crt"));
        assert!(cfg.links[0].kme_client_tls(None).is_none(), "en claro");
    }

    /// Una credencial de KME a medias es casi seguro un typo: mejor no
    /// arrancar que caer en silencio a la autoridad equivocada. Y en un
    /// enlace PQC no hay KME al que presentarse.
    #[test]
    fn partial_or_misplaced_kme_credentials_are_rejected() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"qkd\"\nquditto_url = \"https://kme:20010\"\n\
             kme_cert = \"/kme-a/client.crt\"\n"
        ));
        let err = cfg.validate().expect_err("1 de 3 no vale");
        assert!(
            format!("{err}").contains("kme_cert/kme_key/kme_ca"),
            "{err}"
        );

        let cfg = parse(&format!(
            "{BASE}sdn_url = \"http://10.0.0.100:19002\"\n\
             [[links]]\nneighbor_id = 2\nlink_type = \"pqc\"\nkme_ca = \"/kme-a/ca.crt\"\n"
        ));
        let err = cfg.validate().expect_err("kme_* en un enlace PQC");
        assert!(format!("{err}").contains("no KME here"), "{err}");
    }

    /// A4 (opción B): en un enlace QKD el default con identidad de nodo es el
    /// MISMO que en PQC — handshake firmado (`sign`) + sello `require`, con la
    /// raíz en la época del handshake (las claves de DATOS siguen siendo del
    /// KME, que sigue siendo la raíz de confianza del material). Sin identidad,
    /// `off` histórico; explícito manda.
    #[test]
    fn qkd_link_auth_also_defaults_on_with_identity() {
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"qkd\"\nquditto_url = \"http://kme:20010\"\n"
        ));
        let link = &cfg.links[0];
        assert_eq!(link.effective_pqc_auth(true), PqcAuth::Sign);
        assert_eq!(link.effective_frame_auth(true), FrameAuth::Require);
        assert_eq!(
            link.effective_pqc_auth(false),
            PqcAuth::Off,
            "sin identidad"
        );
        assert_eq!(link.effective_frame_auth(false), FrameAuth::Off);
        // Un valor explícito se respeta — incluido el opt-out.
        let cfg = parse(&format!(
            "{BASE}[[links]]\nneighbor_id = 2\nneighbor_peer_addr = \"127.0.0.1:7002\"\n\
             link_type = \"qkd\"\nquditto_url = \"http://kme:20010\"\n\
             pqc_auth = \"off\"\nframe_auth = \"off\"\n"
        ));
        assert_eq!(cfg.links[0].effective_pqc_auth(true), PqcAuth::Off);
        assert_eq!(cfg.links[0].effective_frame_auth(true), FrameAuth::Off);
    }
}
