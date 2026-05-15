//! Configuración de quditto.
//!
//! Todos los parámetros del modelo físico vienen por CLI con defaults
//! razonables. No hay `default.toml` ni env vars — quditto es un
//! binario pequeño que se arranca con un comando directo.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QudittoConfig {
    /// Dirección de escucha HTTP (ETSI 014). Formato `host:port`.
    pub listen: String,

    /// `R₀` — tasa base de generación (keys/s) cuando `distance = 0`.
    pub r0: f64,

    /// `α` — coeficiente de atenuación en dB/km.
    pub alpha: f64,

    /// `d` — distancia del enlace simulado en km.
    pub distance_km: f64,

    /// Capacidad máxima del buffer en número de claves.
    /// Si está lleno, los ticks de generación se descartan.
    pub max_buffer_keys: u64,

    /// Tamaño de la clave servida en bits. Por la spec es 256.
    pub key_size_bits: u32,
}

impl QudittoConfig {
    /// Validaciones mínimas: rangos positivos donde corresponde.
    pub fn validate(&self) -> Result<(), String> {
        if self.r0 <= 0.0 {
            return Err(format!("r0 must be > 0, got {}", self.r0));
        }
        if self.alpha < 0.0 {
            return Err(format!("alpha must be >= 0, got {}", self.alpha));
        }
        if self.distance_km < 0.0 {
            return Err(format!("distance must be >= 0, got {}", self.distance_km));
        }
        if self.max_buffer_keys == 0 {
            return Err("max_buffer_keys must be > 0".into());
        }
        if self.key_size_bits == 0 || !self.key_size_bits.is_multiple_of(8) {
            return Err(format!(
                "key_size_bits must be a positive multiple of 8, got {}",
                self.key_size_bits
            ));
        }
        Ok(())
    }
}
