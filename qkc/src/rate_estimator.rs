//! Estimación in situ de la tasa de generación del enlace QKD.
//!
//! `r0/alpha/distance_km` son parámetros del *simulador*: en un despliegue
//! real nadie los conoce, y la SDN dimensionaba la arista con un número
//! inventado. Este módulo mide la tasa real a través de lo único que todo
//! KME expone — el `/status` ETSI-014 (`stored_key_count`) más las entregas
//! que ya contamos — y el QKC la anuncia a la SDN en su latido.
//!
//! ## El principio: ley de conservación con censura
//!
//! Por intervalo entre dos sondeos del stock del KME:
//!
//! ```text
//! producido = ΔS + drenado(mis enc_keys + los enc del peer, vía su NOTIFY)
//! ```
//!
//! El drenaje del peer importa: en quditto (FIFO `fresh` compartido) sus
//! `enc_keys` bajan el mismo stock que sondeamos; en el mundo real de dos
//! KMEs, nuestro `dec_keys` de esas mismas claves drena el nuestro. En
//! ambos casos el conteo correcto es **al llegar su `FRAME_KEY_IDS_NOTIFY`**
//! (milisegundos después de su `enc_keys`), NO al completar nuestro
//! `dec_keys`: el worker DEC va segundos por detrás durante el fill, y ese
//! retardo dejaba el ΔS del drenaje del peer sin compensar dentro de las
//! ventanas válidas — medido −15 % clavados en la primera corrida en vivo
//! (1341 estimadas vs 1588,7 reales, 2026-09-01).
//!
//! El único intervalo inválido es el **censurado**: si el stock tocó (o pudo
//! tocar) el techo `max_key_count` durante el intervalo, el KME descartó o
//! pausó producción y el balance ya no la ve. Esos intervalos se descartan;
//! el suelo (`stored = 0`) en cambio es seguro — con el buffer seco, todo
//! lo producido nos lo llevamos, y eso ES la tasa.
//!
//! ## Lo que ningún estimador puede hacer
//!
//! Con todos los buffers llenos y tráfico cero, la tasa es inobservable
//! (la información se destruye en el KME). Ahí el estimador **mantiene la
//! última estimación** con calidad [`RateQuality::Floor`] — nunca decae a 0
//! por falta de datos, que es lo que rompería el lazo
//! medida→SDN→rates→tráfico→medida. El `KeyStore` reduce ese punto ciego
//! manteniendo el stock del KME lejos del techo mientras su anillo ENC
//! tenga hueco (ver `bank_from` en `keystore.rs`): material que el techo
//! habría destruido se banca aguas abajo, y de paso el enlace queda en la
//! banda observable.
//!
//! ## Ventanas por cuenta, media por horizonte, reset por cambio de nivel
//!
//! El hardware real entrega clave destilada en bloques (amplificación de
//! privacidad): `S(t)` es una escalera, y ventanas de tiempo fijo leerían 0
//! entre bloques. Se acumula hasta [`WINDOW_MIN_KEYS`] claves observadas
//! (o [`WINDOW_MAX_SECS`], lo primero).
//!
//! La estimación es la **media ponderada por tiempo** (`Σclaves/Σsegundos`)
//! de las ventanas de un horizonte de [`HORIZON_SECS`]: inmune por
//! construcción al aliasing bloque↔sondeo. Ojo con "mejorarla": la primera
//! versión metía las tasas por-ventana en una mediana, y con bloques de
//! 1,28 s sondeados a 1 Hz las ventanas salen bimodales (256/s ó 128/s) —
//! la mediana elige la moda, no la media: **+27 % clavados en vivo**
//! (2026-09-01). La media temporal da 0,2 % en el mismo escenario.
//!
//! Un cambio de nivel real no debe esperar a que el horizonte se diluya:
//! si la media corta (≥ [`SHORT_MIN_SECS`] s) se desvía > [`SNAP_DEVIATION`]
//! de la larga durante varias ventanas seguidas, el horizonte se trunca al
//! tramo corto. Asimetría donde importa: a la baja bastan [`RUN_DOWN`]
//! ventanas (sobreestimar es el error caro — la SDN repartiría caudal que
//! la fibra no sostiene), al alza se piden [`RUN_UP`].

use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use parking_lot::Mutex;

/// Claves observadas que cierran una ventana. A R=1500 con sondeo a 1 Hz la
/// ventana es ~1 sondeo; a R=25 son ~10 s (la corta [`WINDOW_MAX_SECS`]).
pub const WINDOW_MIN_KEYS: f64 = 256.0;
/// Tope de duración de una ventana: acota la vejez de la muestra con tasas
/// muy bajas.
pub const WINDOW_MAX_SECS: f64 = 10.0;
/// Horizonte de la media larga: ventanas más viejas se olvidan.
const HORIZON_SECS: f64 = 30.0;
/// La media corta cubre las últimas ventanas hasta sumar al menos esto —
/// varias veces el periodo de bloque típico, o el desvío corto-vs-largo
/// dispararía resets espurios por puro aliasing.
const SHORT_MIN_SECS: f64 = 5.0;
/// Desvío relativo corto-vs-largo que cuenta para el reset.
const SNAP_DEVIATION: f64 = 0.25;
/// Ventanas seguidas desviadas A LA BAJA que truncan el horizonte al tramo
/// corto (rápido: sobreestimar es el error caro).
const RUN_DOWN: u32 = 3;
/// Ventanas seguidas desviadas AL ALZA para lo mismo (más lento: una racha
/// alta no debe inflar la capacidad).
const RUN_UP: u32 = 6;
/// Una estimación con última ventana válida más vieja que esto deja de ser
/// `Measured` y pasa a `Floor` (sigue valiendo como cota inferior).
const FRESH_TTL: Duration = Duration::from_secs(15);
/// Errores seguidos de `/status` que marcan el KME como inalcanzable.
const ERR_STREAK_UNAVAILABLE: u32 = 5;
/// Banda de guarda mínima contra el techo, en claves. Si al principio o al
/// final del intervalo el stock está a menos de la guarda del techo, el
/// intervalo pudo perder producción y se censura.
const CEILING_GUARD_MIN: u64 = 8;
/// Un hueco entre sondeos mayor que esto (proceso parado, KME colgado) no es
/// un intervalo: se descarta y se re-ancla.
const MAX_SAMPLE_GAP_SECS: f64 = 30.0;

/// Calidad de la estimación, en orden de confianza.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateQuality {
    /// Muestras válidas recientes: el número es una medición.
    Measured,
    /// Sin ventana válida reciente (buffers llenos / enlace ocioso): el
    /// número es la última medición, válida como **cota inferior**. Nunca
    /// se decae a 0 desde aquí.
    Floor,
    /// El KME no responde: el número es el último conocido y probablemente
    /// esté obsoleto.
    Unavailable,
}

impl RateQuality {
    pub fn as_str(&self) -> &'static str {
        match self {
            RateQuality::Measured => "measured",
            RateQuality::Floor => "floor",
            RateQuality::Unavailable => "unavailable",
        }
    }
}

/// Estimación publicable de la tasa del enlace.
#[derive(Debug, Clone, Copy)]
pub struct RateReport {
    pub keys_per_s: f64,
    pub quality: RateQuality,
    /// Edad de la última ventana válida.
    pub age: Duration,
}

#[derive(Debug, Clone, Copy)]
struct StockPoint {
    stored: u64,
    pulled_total: u64,
    at: Instant,
}

/// Ventana cerrada: claves observadas y tiempo observado.
#[derive(Debug, Clone, Copy)]
struct WindowRec {
    keys: f64,
    secs: f64,
    at: Instant,
}

#[derive(Debug, Default)]
struct Inner {
    last: Option<StockPoint>,
    win_keys: f64,
    win_secs: f64,
    /// Ventanas válidas del horizonte, viejas delante.
    ring: VecDeque<WindowRec>,
    est: Option<f64>,
    last_valid_at: Option<Instant>,
    run_down: u32,
    run_up: u32,
    err_streak: u32,
}

/// Estimador por enlace QKD. Los workers del `KeyStore` cuentan drenajes
/// (lock-free); el sondeador de stock alimenta `on_stock` ~1/s.
#[derive(Debug, Default)]
pub struct RateEstimator {
    /// Total acumulado de claves drenadas del almacén (mis `enc_keys` + los
    /// del peer vía NOTIFY).
    pulled: AtomicU64,
    inner: Mutex<Inner>,
}

impl RateEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    /// El KME nos entregó `n` claves por `enc_keys`.
    pub fn on_delivered(&self, n: usize) {
        self.pulled.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// El peer anunció `n` claves por NOTIFY: su `enc_keys` ya drenó el
    /// almacén (el compartido en quditto; el nuestro, vía nuestro `dec_keys`
    /// inminente, con dos KMEs). Se cuenta AQUÍ y no en la entrega `dec` —
    /// ver el doc del módulo.
    pub fn on_peer_drained(&self, n: usize) {
        self.pulled.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// El KME devolvió `n` claves MENOS de las que el peer anunció: esos ids
    /// no existían (o ya no). Se deshace su cuenta (C-10): sin esto un vecino
    /// inflaba nuestra medida anunciando ids inventados y fijaba él solo la
    /// capacidad de la arista.
    pub fn on_peer_drained_shortfall(&self, n: usize) {
        let n = n as u64;
        let _ = self
            .pulled
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(n))
            });
    }

    /// Un sondeo de `/status` falló (KME caído, TLS, timeout).
    pub fn on_stock_error(&self) {
        let mut g = self.inner.lock();
        g.err_streak = g.err_streak.saturating_add(1);
        // Sin lectura de stock el intervalo en curso ya no cierra balance:
        // re-ancla en el siguiente sondeo bueno.
        g.last = None;
        g.win_keys = 0.0;
        g.win_secs = 0.0;
    }

    /// Un sondeo de `/status` con éxito. `now` explícito para poder testear
    /// sin reloj de verdad.
    pub fn on_stock(&self, stored: u64, max: u64, now: Instant) {
        let pulled_total = self.pulled.load(Ordering::Relaxed);
        let mut g = self.inner.lock();
        g.err_streak = 0;
        let point = StockPoint {
            stored,
            pulled_total,
            at: now,
        };
        let Some(prev) = g.last.replace(point) else {
            return;
        };
        let dt = now.saturating_duration_since(prev.at).as_secs_f64();
        if dt <= 0.0 || dt > MAX_SAMPLE_GAP_SECS {
            g.win_keys = 0.0;
            g.win_secs = 0.0;
            return;
        }

        // Censura: si cualquiera de los dos extremos del intervalo está a
        // menos de la guarda del techo, el KME pudo descartar (o pausar)
        // producción a mitad y el balance la perdería. La guarda escala con
        // la propia estimación (lo que cabría producir en `dt` y no ver),
        // acotada para no censurarlo todo con buffers pequeños.
        let guard = Self::ceiling_guard(g.est, dt, max);
        if stored.saturating_add(guard) >= max || prev.stored.saturating_add(guard) >= max {
            g.win_keys = 0.0;
            g.win_secs = 0.0;
            return;
        }

        let pulled = pulled_total.saturating_sub(prev.pulled_total);
        let contrib = (stored as f64 - prev.stored as f64) + pulled as f64;
        g.win_keys += contrib;
        g.win_secs += dt;
        if g.win_keys >= WINDOW_MIN_KEYS || g.win_secs >= WINDOW_MAX_SECS {
            if g.win_secs > 0.0 {
                let keys = g.win_keys.max(0.0);
                let secs = g.win_secs;
                g.accept_window(keys, secs, now);
                tracing::debug!(
                    win_keys = format!("{keys:.0}"),
                    win_secs = format!("{secs:.2}"),
                    est = g.est.map(|e| format!("{e:.1}")),
                    ring = g.ring.len(),
                    "rate_estimator.window"
                );
            }
            g.win_keys = 0.0;
            g.win_secs = 0.0;
        }
    }

    fn ceiling_guard(est: Option<f64>, dt: f64, max: u64) -> u64 {
        let by_rate = est.map_or(0.0, |e| e * dt * 1.5) as u64;
        by_rate.max(CEILING_GUARD_MIN).min(max / 4)
    }

    /// Estimación actual, o `None` si nunca hubo ventana válida (fuente sin
    /// stock — PQC — o enlace recién nacido). El anunciante omite el campo.
    pub fn report(&self) -> Option<RateReport> {
        self.report_at(Instant::now())
    }

    pub fn report_at(&self, now: Instant) -> Option<RateReport> {
        let g = self.inner.lock();
        let est = g.est?;
        let last = g.last_valid_at?;
        let age = now.saturating_duration_since(last);
        let quality = if g.err_streak >= ERR_STREAK_UNAVAILABLE {
            RateQuality::Unavailable
        } else if age <= FRESH_TTL {
            RateQuality::Measured
        } else {
            RateQuality::Floor
        };
        Some(RateReport {
            keys_per_s: est,
            quality,
            age,
        })
    }
}

fn sums<'a>(it: impl Iterator<Item = &'a WindowRec>) -> (f64, f64) {
    it.fold((0.0, 0.0), |(k, s), w| (k + w.keys, s + w.secs))
}

impl Inner {
    fn accept_window(&mut self, keys: f64, secs: f64, now: Instant) {
        self.ring.push_back(WindowRec {
            keys,
            secs,
            at: now,
        });
        while let Some(front) = self.ring.front() {
            if now.saturating_duration_since(front.at).as_secs_f64() > HORIZON_SECS {
                self.ring.pop_front();
            } else {
                break;
            }
        }

        let (lk, ls) = sums(self.ring.iter());
        if ls <= 0.0 {
            return;
        }
        let long = lk / ls;

        // Tramo corto: las últimas ventanas hasta cubrir SHORT_MIN_SECS.
        let mut n_short = 0;
        let mut sk = 0.0;
        let mut ss = 0.0;
        for w in self.ring.iter().rev() {
            sk += w.keys;
            ss += w.secs;
            n_short += 1;
            if ss >= SHORT_MIN_SECS {
                break;
            }
        }
        let short = if ss > 0.0 { sk / ss } else { long };

        // Detección de cambio de nivel: solo tiene sentido si el horizonte
        // contiene historia MÁS ALLÁ del tramo corto.
        if ls > ss + 1e-9 {
            let hi = long * (1.0 + SNAP_DEVIATION);
            let lo = long * (1.0 - SNAP_DEVIATION);
            if short < lo {
                self.run_down += 1;
                self.run_up = 0;
            } else if short > hi {
                self.run_up += 1;
                self.run_down = 0;
            } else {
                self.run_down = 0;
                self.run_up = 0;
            }
            if self.run_down >= RUN_DOWN || self.run_up >= RUN_UP {
                // El nivel cambió: la historia previa ya no describe el
                // enlace. Quédate con el tramo corto.
                while self.ring.len() > n_short {
                    self.ring.pop_front();
                }
                self.run_down = 0;
                self.run_up = 0;
                self.est = Some(short);
                self.last_valid_at = Some(now);
                return;
            }
        } else {
            self.run_down = 0;
            self.run_up = 0;
        }

        self.est = Some(long);
        self.last_valid_at = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    /// Simula `secs` segundos de sondeos a 1 Hz contra un KME de tasa `rate`
    /// y buffer `max`, drenado por el consumidor a `drain` claves/s (lo que
    /// pedimos nos lo llevamos si hay stock). Devuelve el instante final.
    fn run_kme(
        est: &RateEstimator,
        start: Instant,
        secs: u64,
        rate: f64,
        drain: f64,
        max: u64,
        stored0: &mut f64,
    ) -> Instant {
        let mut now = start;
        for _ in 0..secs {
            now += Duration::from_secs(1);
            // produce (recortado al techo), luego drena (recortado al stock)
            *stored0 = (*stored0 + rate).min(max as f64);
            let take = drain.min(*stored0);
            *stored0 -= take;
            est.on_delivered(take.round() as usize);
            est.on_stock(stored0.round() as u64, max, now);
        }
        now
    }

    #[test]
    fn converges_under_steady_consumption() {
        let est = RateEstimator::new();
        let mut stored = 500.0;
        let now = run_kme(&est, t0(), 30, 1000.0, 1000.0, 8192, &mut stored);
        let r = est.report_at(now).expect("hay estimación");
        assert!(
            (r.keys_per_s - 1000.0).abs() < 50.0,
            "est={} lejos de 1000",
            r.keys_per_s
        );
        assert_eq!(r.quality, RateQuality::Measured);
    }

    /// Ocioso con hueco: nadie consume, el stock sube — la subida ES la tasa.
    /// Es la ventana del arranque en frío y la que rompe el lazo
    /// medida→rates→tráfico→medida.
    #[test]
    fn idle_fill_measures_production_without_consuming() {
        let est = RateEstimator::new();
        let mut stored = 0.0;
        // 8192 de buffer a 1588 keys/s: ~5 s de subida limpia antes del techo.
        let now = run_kme(&est, t0(), 4, 1588.0, 0.0, 8192, &mut stored);
        let r = est.report_at(now).expect("la subida sola debe medir");
        assert!(
            (r.keys_per_s - 1588.0).abs() < 80.0,
            "est={} lejos de 1588",
            r.keys_per_s
        );
    }

    /// Buffer clavado en el techo: producción invisible. La estimación NO
    /// baja (queda como suelo) — jamás decaer a 0 por falta de datos.
    #[test]
    fn full_buffer_censors_and_holds_as_floor() {
        let est = RateEstimator::new();
        let mut stored = 100.0;
        let mut now = run_kme(&est, t0(), 20, 1000.0, 1000.0, 8192, &mut stored);
        let before = est.report_at(now).unwrap().keys_per_s;
        // 60 s con el buffer al techo y drenaje cero.
        stored = 8192.0;
        now = run_kme(&est, now, 60, 1000.0, 0.0, 8192, &mut stored);
        let r = est.report_at(now).unwrap();
        assert!(
            (r.keys_per_s - before).abs() < f64::EPSILON,
            "la censura movió la estimación: {} → {}",
            before,
            r.keys_per_s
        );
        assert_eq!(r.quality, RateQuality::Floor);
    }

    /// Una caída real (fibra doblada, QBER) se sigue deprisa: sobreestimar
    /// es el error caro.
    #[test]
    fn step_down_is_tracked_fast() {
        let est = RateEstimator::new();
        let mut stored = 500.0;
        let mut now = run_kme(&est, t0(), 30, 1000.0, 1000.0, 8192, &mut stored);
        now = run_kme(&est, now, 12, 300.0, 1000.0, 8192, &mut stored);
        let r = est.report_at(now).unwrap();
        assert!(
            r.keys_per_s < 450.0,
            "12 s tras caer a 300 sigue en {}",
            r.keys_per_s
        );
    }

    /// Una mejora sostenida trunca el horizonte (RUN_UP) — no se queda
    /// castigada por la media larga vieja.
    #[test]
    fn sustained_step_up_snaps() {
        let est = RateEstimator::new();
        let mut stored = 500.0;
        let mut now = run_kme(&est, t0(), 30, 500.0, 500.0, 8192, &mut stored);
        now = run_kme(&est, now, 40, 2000.0, 2000.0, 8192, &mut stored);
        let r = est.report_at(now).unwrap();
        assert!(
            r.keys_per_s > 1500.0,
            "40 s tras subir a 2000 sigue en {}",
            r.keys_per_s
        );
    }

    /// Entrega por bloques (escalera): la media temporal por horizonte es
    /// inmune al aliasing bloque↔sondeo. La versión con mediana daba +27 %
    /// aquí (elige la moda de ventanas bimodales) — no reintroducirla.
    #[test]
    fn block_delivery_still_converges() {
        let est = RateEstimator::new();
        let max = 8192u64;
        let mut now = t0();
        let mut stored = 0u64;
        let rate = 256.0; // 512 claves cada 2 s, drenadas a la misma media
        for i in 0..120u64 {
            now += Duration::from_secs(1);
            if i % 2 == 1 {
                stored = (stored + 512).min(max);
            }
            let take = stored.min(256);
            stored -= take;
            est.on_delivered(take as usize);
            est.on_stock(stored, max, now);
        }
        let r = est.report_at(now).expect("hay estimación");
        assert!(
            (r.keys_per_s - rate).abs() < rate * 0.10,
            "est={} lejos de {rate}",
            r.keys_per_s
        );
    }

    /// Aliasing del brazo C en vivo: bloques cada 1,28 s sondeados a 1 Hz
    /// producen ventanas bimodales (1 s → 256/s, 2 s → 128/s). La media
    /// temporal debe dar la tasa real (200), no la moda (256).
    #[test]
    fn bimodal_windows_from_aliasing_do_not_bias() {
        let est = RateEstimator::new();
        let max = 8192u64;
        let mut now = t0();
        let mut produced = 0.0_f64;
        let mut stored = 0.0_f64;
        let rate = 200.0; // bloques de 256 → cadencia 1,28 s
        for _ in 0..90u64 {
            now += Duration::from_secs(1);
            produced += rate;
            while produced >= 256.0 {
                stored += 256.0;
                produced -= 256.0;
            }
            let take = stored.min(200.0);
            stored -= take;
            est.on_delivered(take as usize);
            est.on_stock(stored as u64, max, now);
        }
        let r = est.report_at(now).expect("hay estimación");
        assert!(
            (r.keys_per_s - rate).abs() < rate * 0.10,
            "est={} sesgada frente a {rate} (¿mediana otra vez?)",
            r.keys_per_s
        );
    }

    #[test]
    fn unreachable_kme_flags_and_recovers() {
        let est = RateEstimator::new();
        let mut stored = 500.0;
        let mut now = run_kme(&est, t0(), 20, 1000.0, 1000.0, 8192, &mut stored);
        for _ in 0..ERR_STREAK_UNAVAILABLE {
            est.on_stock_error();
        }
        assert_eq!(
            est.report_at(now).unwrap().quality,
            RateQuality::Unavailable
        );
        now = run_kme(&est, now, 20, 1000.0, 1000.0, 8192, &mut stored);
        assert_eq!(est.report_at(now).unwrap().quality, RateQuality::Measured);
    }

    /// Sin stock jamás sondeado (fuente PQC) no hay informe: el anunciante
    /// no manda el campo y la SDN sigue con su modelo declarado.
    #[test]
    fn no_samples_no_report() {
        let est = RateEstimator::new();
        est.on_delivered(1000);
        assert!(est.report().is_none());
    }
}
