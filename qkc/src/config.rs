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

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Un entry por enlace QKC↔QKC con este vecino directo.
    #[serde(default)]
    pub links: Vec<LinkConfig>,
}

fn default_announce_secs() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

    /// Parameter set ML-KEM para el handshake de los enlaces PQC. Ignorado
    /// en enlaces QKD. Default: `ml-kem-768`. Ver
    /// [`common::crypto::pqc::suite`].
    #[serde(default = "default_pqc_suite")]
    pub pqc_suite: String,

    /// Tamaño de la clave OTP en bits. Default: 1024 (mejor amortización
    /// del HTTP a quditto que con 256, ya que 1 clave cubre 128 B de
    /// payload). En enlaces PQC fija la longitud del material derivado por
    /// clave; debe ser idéntico en ambos extremos.
    #[serde(default = "default_key_size")]
    pub key_size_bits: u32,

    /// **Re-keying PQC** (solo enlaces PQC, ignorado en QKD). Rota el secreto
    /// ML-KEM cada `pqc_rekey_keys` claves emitidas. `0` = sin disparo por
    /// volumen. Default 1000. Acota el blast-radius por secreto.
    #[serde(default = "default_rekey_keys")]
    pub pqc_rekey_keys: u64,

    /// **Re-keying PQC**: rota el secreto cada `pqc_rekey_secs` segundos (tope
    /// de edad, p. ej. enlaces de poco tráfico). `0` = sin disparo por tiempo.
    /// Default 3600 (1 h). La rotación ocurre en `max(pqc_rekey_keys,
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
}
