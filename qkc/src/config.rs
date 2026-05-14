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
//! [[links]]
//! neighbor_id        = 2
//! neighbor_peer_addr = "127.0.0.1:7002"
//! quditto_url        = "http://127.0.0.1:8081"   # el quditto compartido del enlace 1↔2
//! key_size_bits      = 256
//!
//! [[links]]
//! neighbor_id        = 3
//! neighbor_peer_addr = "127.0.0.1:7003"
//! quditto_url        = "http://127.0.0.1:8082"   # quditto distinto, del enlace 1↔3
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::QkcError;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QkcConfig {
    pub qkc_id: u32,

    /// Donde escucho frames de **otros QKCs**.
    pub peer_listen: String,

    /// Donde escucho frames del **ORR co-localizado**.
    pub local_listen: String,

    /// Donde monto el HTTP admin (forwarding-table push, healthz).
    pub admin_http: String,

    /// Un entry por enlace QKC↔QKC con este vecino directo.
    #[serde(default)]
    pub links: Vec<LinkConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkConfig {
    /// ID del QKC vecino al otro lado del enlace.
    pub neighbor_id: u32,

    /// `host:port` del listener TCP-peer de ese QKC.
    pub neighbor_peer_addr: String,

    /// URL base del quditto compartido por los dos QKCs (este lado y
    /// el vecino apuntan al mismo). Sirve ETSI 014 en
    /// `/api/v1/keys/{sae_id}/{enc,dec}_keys`.
    pub quditto_url: String,

    /// Tamaño de la clave OTP en bits. Default: 1024 (mejor amortización
    /// del HTTP a quditto que con 256, ya que 1 clave cubre 128 B de
    /// payload).
    #[serde(default = "default_key_size")]
    pub key_size_bits: u32,
}

fn default_key_size() -> u32 {
    1024
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
                    "link.neighbor_id == qkc_id ({})", self.qkc_id
                )));
            }
            if link.key_size_bits == 0 || link.key_size_bits % 8 != 0 {
                return Err(QkcError::BadRequest(format!(
                    "link.key_size_bits must be a positive multiple of 8, got {}",
                    link.key_size_bits
                )));
            }
        }
        Ok(())
    }
}
