//! `QudittoService` — handle compartido entre minter y handlers HTTP.
//!
//! Cheap-to-clone: solo es un `Arc<LinkBuffer>` por dentro.

use std::{sync::Arc, time::Duration};

use tracing::{debug, info};

use crate::{
    config::QudittoConfig,
    crypto::{build_rng, Key},
    error::Result,
    link::LinkBuffer,
};

#[derive(Clone)]
pub struct QudittoService {
    pub link: Arc<LinkBuffer>,
}

impl QudittoService {
    pub fn new(cfg: QudittoConfig) -> Self {
        Self {
            link: Arc::new(LinkBuffer::new(cfg)),
        }
    }

    pub fn cfg(&self) -> &QudittoConfig {
        &self.link.cfg
    }

    /// Generador en background.
    ///
    /// El RNG (`ChaCha20Rng`) se aloja en stack-localmente de la task:
    /// se siembra una sola vez desde `OsRng` y luego no hace syscalls.
    /// Cada tick mintea `round(R · Δt)` claves de golpe y las empuja
    /// al `ArrayQueue` lock-free.
    ///
    /// **Cadencia adaptativa**:
    /// - `R ≥ 10` keys/s → lotes de 100 ms (`R/10` claves por tick).
    /// - `R < 10` keys/s → 1 clave cada `1/R` segundos (clamp ≥100 ms).
    /// - `R == 0` → minter dormido para siempre.
    pub async fn run_minter(self) -> Result<()> {
        let rate = self.link.current_rate_kps();
        info!(
            r0       = self.cfg().r0,
            alpha    = self.cfg().alpha,
            distance = self.cfg().distance_km,
            rate_kps = rate,
            "quditto: minter started",
        );

        if rate <= 0.0 {
            futures_park().await;
            return Ok(());
        }

        let tick_ms: u64 = if rate >= 10.0 {
            100
        } else {
            ((1000.0 / rate).round() as u64).max(100)
        };
        let tick_dur = Duration::from_millis(tick_ms);
        let dt_s = tick_ms as f64 / 1000.0;
        let per_tick = (rate * dt_s).round() as u64;
        debug!(tick_ms, per_tick, "quditto: minter cadence");

        // RNG userspace inicializado una vez. No syscalls en el hot path.
        let mut rng = build_rng();
        let n_bytes = (self.cfg().key_size_bits / 8) as usize;

        let mut ticker = tokio::time::interval(tick_dur);
        // Skip el primer tick instantáneo de `interval` para no minar
        // un lote en t=0.
        ticker.tick().await;

        loop {
            ticker.tick().await;
            for _ in 0..per_tick {
                let pushed = self.link.push(Key::mint(&mut rng, n_bytes));
                if !pushed {
                    // Buffer lleno — el resto del tick va a drop también.
                    break;
                }
            }
        }
    }
}

async fn futures_park() {
    tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
}
