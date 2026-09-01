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

/// Periodo del logger de stats de quditto. Cada 30 s emite un INFO
/// con counters cumulativos + fill instantáneo del FIFO. Útil para
/// diagnosticar:
///   * agotamiento del buffer fresh (= QKC pidiendo más rate del que
///     R₀ × atenuación produce).
///   * drops por buffer lleno (= consumidor lento, fresh sobreproducido).
///   * stalls del minter (= contador `generated` no avanza).
pub const STATS_LOG_PERIOD: Duration = Duration::from_secs(30);

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
    /// La producción se lleva en un acumulador fraccional `due` (claves
    /// devengadas y aún no empujadas), lo que unifica los tres modos:
    ///
    /// **Cadencia adaptativa**:
    /// - `R ≥ 10` keys/s → lotes de 100 ms (`R/10` claves por tick).
    /// - `R < 10` keys/s → 1 clave cada `1/R` segundos (clamp ≥100 ms).
    /// - `R == 0` → minter dormido para siempre.
    ///
    /// **Buffer lleno** (`full_mode`): `drop` mintea y descarta (histórico);
    /// `pause` deja de destilar como el hardware real — `due` se congela en
    /// un bloque como mucho, sin acumular deuda, y `paused` lo cuenta.
    ///
    /// **`block_keys`**: la clave sale en bloques de N (escalera en
    /// `stored_key_count`, como la amplificación de privacidad real).
    ///
    /// **`rate_step`**: a los `at_s` segundos la tasa se multiplica por
    /// `factor` y la cadencia se recalcula — para validar el estimador del
    /// QKC contra un cambio real.
    pub async fn run_minter(self) -> Result<()> {
        let rate0 = self.link.current_rate_kps();
        info!(
            r0 = self.cfg().r0,
            alpha = self.cfg().alpha,
            distance = self.cfg().distance_km,
            rate_kps = rate0,
            full_mode = ?self.cfg().full_mode,
            block_keys = self.cfg().block_keys,
            "quditto: minter started",
        );

        // Lanza logger de stats en background. Vive lo mismo que el
        // proceso (el minter es el último loop infinito de la app).
        let stats_link = self.link.clone();
        tokio::spawn(async move { run_stats_logger(stats_link).await });

        if rate0 <= 0.0 {
            futures_park().await;
            return Ok(());
        }

        let cadence = |rate: f64| -> (Duration, f64) {
            let tick_ms: u64 = if rate >= 10.0 {
                100
            } else {
                ((1000.0 / rate).round() as u64).max(100)
            };
            (
                Duration::from_millis(tick_ms),
                rate * tick_ms as f64 / 1000.0,
            )
        };

        let mut rate = rate0;
        let (mut tick_dur, mut per_tick) = cadence(rate);
        debug!(
            tick_ms = tick_dur.as_millis() as u64,
            per_tick, "quditto: minter cadence"
        );

        // RNG userspace inicializado una vez. No syscalls en el hot path.
        let mut rng = build_rng();
        let n_bytes = (self.cfg().key_size_bits / 8) as usize;
        let full_mode = self.cfg().full_mode;
        // Bloque de entrega; 1 = continuo.
        let block = self.cfg().block_keys.max(1);
        let step = self.cfg().rate_step;
        let start = std::time::Instant::now();
        let mut stepped = false;
        let mut due: f64 = 0.0;

        let mut ticker = tokio::time::interval(tick_dur);
        // Skip el primer tick instantáneo de `interval` para no minar
        // un lote en t=0.
        ticker.tick().await;

        loop {
            ticker.tick().await;

            if let (false, Some(s)) = (stepped, step) {
                if start.elapsed().as_secs_f64() >= s.at_s {
                    stepped = true;
                    rate *= s.factor;
                    info!(
                        rate_kps = rate,
                        factor = s.factor,
                        "quditto: rate step applied"
                    );
                    if rate <= 0.0 {
                        futures_park().await;
                        return Ok(());
                    }
                    (tick_dur, per_tick) = cadence(rate);
                    ticker = tokio::time::interval(tick_dur);
                    ticker.tick().await;
                }
            }

            due += per_tick;
            while due >= block as f64 {
                if full_mode == crate::config::FullMode::Pause && self.link.fresh_space() < block {
                    // Pausa: se deja de destilar. La deuda no crece más allá
                    // del bloque en curso — al liberarse hueco se reanuda a
                    // tasa R, sin ráfaga de "backlog" que nunca existió.
                    let cap = block as f64;
                    if due > cap {
                        self.link.note_paused((due - cap) as u64);
                        due = cap;
                    }
                    break;
                }
                for _ in 0..block {
                    // En `drop` el push cuenta el descarte; en `pause` ya
                    // comprobamos hueco para el bloque entero.
                    let _ = self.link.push(Key::mint(&mut rng, n_bytes));
                }
                due -= block as f64;
            }
        }
    }
}

async fn futures_park() {
    tokio::time::sleep(Duration::from_secs(u64::MAX / 2)).await;
}

/// Logger periódico de stats del enlace. Muestrea cumulativos y los
/// diferencia respecto al sample anterior para emitir tasas instantáneas
/// (keys/s en la ventana de [`STATS_LOG_PERIOD`]).
async fn run_stats_logger(link: Arc<LinkBuffer>) {
    let mut prev = link.stats_snapshot();
    let mut tick = tokio::time::interval(STATS_LOG_PERIOD);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await; // saltar el tick inmediato
    loop {
        tick.tick().await;
        let cur = link.stats_snapshot();
        let dt = STATS_LOG_PERIOD.as_secs_f64();
        let gen_rate = (cur.generated.saturating_sub(prev.generated)) as f64 / dt;
        let drop_rate = (cur.dropped.saturating_sub(prev.dropped)) as f64 / dt;
        let paused_rate = (cur.paused.saturating_sub(prev.paused)) as f64 / dt;
        let enc_rate = (cur.delivered_enc.saturating_sub(prev.delivered_enc)) as f64 / dt;
        let dec_rate = (cur.delivered_dec.saturating_sub(prev.delivered_dec)) as f64 / dt;
        info!(
            fresh_available = link.fresh_available(),
            delivered_pending = link.delivered_pending(),
            generated_total = cur.generated,
            dropped_total = cur.dropped,
            delivered_enc_total = cur.delivered_enc,
            delivered_dec_total = cur.delivered_dec,
            gen_kps = format!("{gen_rate:.1}"),
            drop_kps = format!("{drop_rate:.1}"),
            paused_kps = format!("{paused_rate:.1}"),
            enc_serve_kps = format!("{enc_rate:.1}"),
            dec_serve_kps = format!("{dec_rate:.1}"),
            "quditto.stats",
        );
        prev = cur;
    }
}
