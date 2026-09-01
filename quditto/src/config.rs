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
    /// Si está lleno, ver `full_mode`.
    pub max_buffer_keys: u64,

    /// Tamaño de la clave servida en bits. Por la spec es 256.
    pub key_size_bits: u32,

    /// Qué pasa con la producción cuando el buffer está lleno:
    /// * `drop` (default, comportamiento histórico) — se mintea y descarta.
    /// * `pause` — se deja de destilar hasta que haya hueco, como el
    ///   hardware QKD real. Para el consumidor ambos son idénticos
    ///   (producción invisible); el modo existe para que el estimador de
    ///   tasa del QKC se valide contra las dos variantes.
    pub full_mode: FullMode,

    /// Entrega en bloques de N claves (amplificación de privacidad del
    /// hardware real: `stored_key_count` sube en escalera, no en rampa).
    /// `0` = continuo (histórico).
    pub block_keys: u64,

    /// Escalón de tasa para tests del estimador: a los `at_s` segundos del
    /// arranque, la tasa se multiplica por `factor`. `None` = tasa fija.
    pub rate_step: Option<RateStep>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FullMode {
    #[default]
    Drop,
    Pause,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct RateStep {
    pub at_s: f64,
    pub factor: f64,
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
        if self.block_keys > self.max_buffer_keys {
            return Err(format!(
                "block_keys ({}) no cabe en max_buffer_keys ({}): ningún bloque entraría nunca",
                self.block_keys, self.max_buffer_keys
            ));
        }
        if let Some(s) = &self.rate_step {
            if s.at_s < 0.0 || !s.factor.is_finite() || s.factor <= 0.0 {
                return Err(format!(
                    "rate_step inválido: at_s={} factor={}",
                    s.at_s, s.factor
                ));
            }
        }
        Ok(())
    }
}
