//! Token bucket por `(peer_dkms_id, sae_id)` con límites dinámicos.
//!
//! Réplica del modelo Python (`code_dkms/src/DKMS/control/token_bucket.py`):
//!
//! ```text
//!   refill_rate = link_capacity / N_active_SAEs
//!   capacity    = max(refill_rate × observation_window,
//!                     buffer_occupancy / N_active_SAEs,
//!                     min_capacity)
//! ```
//!
//! Cada `try_admit` recalcula límites para reflejar el N_SAEs y la
//! ocupación actuales — un SAE recién llegado reduce el burst que el
//! resto puede consumir.
//!
//! Eviction: SAEs sin peticiones en la ventana de observación pierden
//! su bucket y dejan de contar en `active_sae_count`. Esto deja que el
//! resto recupere su porción de burst.
//!
//! `link_capacity` viene del SDN (rate asignada por MCF a este DKMS
//! hacia el peer). Compartimos el `Arc<Mutex<HashMap<String, f64>>>`
//! con el Generator que ya lo pollea.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing::debug;

use common::ids::SaeId;

use crate::state::BufferPool;

/// Estado interno de un bucket. `tokens` y `capacity` son `f64` para
/// soportar rates fraccionarias.
#[derive(Debug)]
struct Bucket {
    refill_rate: f64,
    capacity: f64,
    tokens: f64,
    last_refill: Instant,
}

impl Bucket {
    fn new(now: Instant, refill_rate: f64, capacity: f64) -> Self {
        // Empezamos con bucket lleno: el primer cliente que ve un buffer
        // nuevo no debe verse rate-limited instantáneamente.
        Self {
            refill_rate,
            capacity,
            tokens: capacity,
            last_refill: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now
            .saturating_duration_since(self.last_refill)
            .as_secs_f64();
        if elapsed > 0.0 && self.refill_rate > 0.0 {
            self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.capacity);
        }
        self.last_refill = now;
    }

    /// Refresca el rate/capacity con los valores nuevos calculados en
    /// función del N_SAEs y la ocupación. Aplica también un refill
    /// implícito antes del cambio para no perder tokens acumulados.
    ///
    /// Si el bucket era recién creado (refill_rate=0, capacity=0) los
    /// tokens se inicializan a `capacity` — semántica "arranca lleno"
    /// que evita que la primera request tras crear el bucket vea 0
    /// tokens y reciba 429.
    fn update_limits(&mut self, now: Instant, refill_rate: f64, capacity: f64) {
        let was_uninitialized = self.refill_rate == 0.0 && self.capacity == 0.0;
        self.refill(now);
        self.refill_rate = refill_rate;
        self.capacity = capacity;
        if was_uninitialized || self.tokens > capacity {
            self.tokens = capacity;
        }
    }

    fn try_consume(&mut self, now: Instant, cost: f64) -> Result<(), f64> {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            Ok(())
        } else {
            Err(self.tokens)
        }
    }

    fn refund(&mut self, amount: f64) {
        self.tokens = (self.tokens + amount).min(self.capacity);
    }
}

/// Manager global. Compartido por toda la `DkmsService`.
pub struct SaeBufferBuckets {
    pool: Arc<BufferPool>,
    /// Rates SDN-asignadas por peer DKMS (compartido con el `Generator`).
    sdn_rates: Arc<Mutex<HashMap<String, f64>>>,
    observation_window: Duration,
    min_capacity: f64,

    /// `(peer_dkms_id, sae_id) → Bucket`.
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    buckets: HashMap<(String, String), Bucket>,
    last_request: HashMap<(String, String), Instant>,
    last_eviction: Option<Instant>,
}

impl SaeBufferBuckets {
    pub fn new(
        pool: Arc<BufferPool>,
        sdn_rates: Arc<Mutex<HashMap<String, f64>>>,
        observation_window_secs: f64,
        min_capacity: f64,
    ) -> Self {
        Self {
            pool,
            sdn_rates,
            observation_window: Duration::from_secs_f64(observation_window_secs.max(1.0)),
            min_capacity,
            state: Mutex::new(State::default()),
        }
    }

    /// Cuenta SAEs activos contra `peer` en la ventana de observación.
    /// El propio SAE solicitante se incluye (registrado vía
    /// `register_request` antes de llamar). Mínimo siempre 1 para
    /// evitar division por cero.
    fn active_sae_count(&self, peer: &str, state: &State, now: Instant) -> usize {
        let n = state
            .last_request
            .iter()
            .filter(|((p, _), t)| {
                p.as_str() == peer && now.saturating_duration_since(**t) <= self.observation_window
            })
            .count();
        n.max(1)
    }

    fn ensure_bucket<'a>(
        state: &'a mut State,
        peer: &str,
        sae: &str,
        now: Instant,
    ) -> &'a mut Bucket {
        let key = (peer.to_string(), sae.to_string());
        state
            .buckets
            .entry(key)
            .or_insert_with(|| Bucket::new(now, 0.0, 0.0))
    }

    /// Marca al SAE como activo contra el peer (hace que cuente en
    /// `active_sae_count` durante la ventana). Idempotente.
    fn register_request(state: &mut State, peer: &str, sae: &str, now: Instant) {
        state
            .last_request
            .insert((peer.to_string(), sae.to_string()), now);
    }

    /// Intenta admitir `cost_per_peer` tokens en cada uno de los buckets
    /// indicados (uno por destino DKMS). Atómico: si algún peer queda
    /// sin tokens, los ya consumidos se rembolsan y se devuelve error
    /// con el peer que falló y los tokens disponibles ahí.
    pub fn try_admit_many(
        &self,
        peers: &[String],
        sae: &SaeId,
        cost_per_peer: f64,
    ) -> Result<(), AdmitFailure> {
        let now = Instant::now();
        let mut state = self.state.lock();
        self.maybe_evict(&mut state, now);
        let mut consumed: Vec<String> = Vec::new();

        // Lecturas que necesito ANTES de poder mutate (rates lock interno).
        let rates_snap: HashMap<String, f64> = self.sdn_rates.lock().clone();

        // Snapshot active counts (necesita mutate state.last_request, que
        // toma referencia mutable — debo registrar request ANTES de leer).
        for peer in peers {
            Self::register_request(&mut state, peer, sae.as_str(), now);
        }

        // Pre-compute (active_count, occupancy) por peer una vez. Esto
        // evita doble borrow de `state` durante la iteración.
        let plan: Vec<(String, usize, f64)> = peers
            .iter()
            .map(|peer| {
                let active = self.active_sae_count(peer, &state, now);
                let occupancy = self.pool.for_peer(peer).enc_len() as f64;
                (peer.clone(), active, occupancy)
            })
            .collect();

        for (peer, active, occupancy) in &plan {
            let link_cap = rates_snap.get(peer.as_str()).copied().unwrap_or(0.0);
            let refill = (link_cap / *active as f64).max(0.0);
            let burst_floor = refill * self.observation_window.as_secs_f64();
            // The model bound (max of refill×window, occupancy/N, min)
            // is what the bucket can *grow to over time*. The instantaneous
            // ceiling, however, is the **real buffer occupancy** — if the
            // generator hasn't produced enough keys yet, the bucket must
            // not promise more than what exists, or the caller pops an
            // empty buffer and the response surfaces as 503
            // (`TransportBufferEmpty`) when semantically it is 429
            // (back-pressure: "rate-limited by buffer fill").
            //
            // We cap the bucket capacity at `occupancy` (real buffer
            // level) so that consumes beyond the buffer never succeed
            // here; instead they emit `RateLimited` → HTTP 429 like the
            // user expects under sustained over-rate.
            let cap_grow = burst_floor
                .max(occupancy / *active as f64)
                .max(self.min_capacity);
            let cap = cap_grow.min(*occupancy);
            let bucket = Self::ensure_bucket(&mut state, peer, sae.as_str(), now);
            bucket.update_limits(now, refill, cap);
            // Defensive pre-check: if the real buffer cannot service the
            // requested cost right now, treat it as 429 directly. This
            // matters when ``min_capacity > 0`` keeps the bucket alive
            // with one stale token even though ``occupancy=0``.
            if *occupancy < cost_per_peer {
                for p in &consumed {
                    if let Some(b) = state
                        .buckets
                        .get_mut(&(p.clone(), sae.as_str().to_string()))
                    {
                        b.refund(cost_per_peer);
                    }
                }
                return Err(AdmitFailure {
                    peer: peer.clone(),
                    available: *occupancy,
                    requested: cost_per_peer,
                    active_sae_count: *active,
                    capacity: cap,
                });
            }
            if let Err(available) = bucket.try_consume(now, cost_per_peer) {
                // Rollback de buckets ya consumidos.
                for p in &consumed {
                    if let Some(b) = state
                        .buckets
                        .get_mut(&(p.clone(), sae.as_str().to_string()))
                    {
                        b.refund(cost_per_peer);
                    }
                }
                return Err(AdmitFailure {
                    peer: peer.clone(),
                    available,
                    requested: cost_per_peer,
                    active_sae_count: *active,
                    capacity: cap,
                });
            }
            consumed.push(peer.clone());
        }
        Ok(())
    }

    /// Devuelve `amount` tokens al bucket `(peer, sae)`. Usado por la
    /// política de fallo (un destino DKMS no ACK-ea → revertimos).
    pub fn refund(&self, peer: &str, sae: &SaeId, amount: f64) {
        let mut state = self.state.lock();
        if let Some(b) = state
            .buckets
            .get_mut(&(peer.to_string(), sae.as_str().to_string()))
        {
            b.refund(amount);
        }
    }

    /// Refund en batch para múltiples peers (failure path multi-DKMS).
    pub fn refund_many(&self, peers: &[String], sae: &SaeId, amount: f64) {
        let mut state = self.state.lock();
        for peer in peers {
            if let Some(b) = state
                .buckets
                .get_mut(&(peer.clone(), sae.as_str().to_string()))
            {
                b.refund(amount);
            }
        }
    }

    /// Snapshot de estado para debug/healthz: (peer, sae, tokens, capacity).
    pub fn snapshot(&self) -> Vec<(String, String, f64, f64)> {
        let state = self.state.lock();
        state
            .buckets
            .iter()
            .map(|((peer, sae), b)| (peer.clone(), sae.clone(), b.tokens, b.capacity))
            .collect()
    }

    /// Elimina buckets cuyo SAE no ha pedido nada en la ventana de
    /// observación. Como mucho una vez por ventana (perezoso).
    fn maybe_evict(&self, state: &mut State, now: Instant) {
        let last = state
            .last_eviction
            .unwrap_or(now - self.observation_window - Duration::from_secs(1));
        if now.saturating_duration_since(last) < self.observation_window {
            return;
        }
        state.last_eviction = Some(now);
        let stale: Vec<(String, String)> = state
            .last_request
            .iter()
            .filter(|(_, t)| now.saturating_duration_since(**t) > self.observation_window)
            .map(|(k, _)| k.clone())
            .collect();
        if stale.is_empty() {
            return;
        }
        for key in &stale {
            state.buckets.remove(key);
            state.last_request.remove(key);
        }
        debug!(removed = stale.len(), "sae_buffer_buckets.evicted_stale");
    }
}

/// Error de admisión: indica qué peer falló y por qué.
#[derive(Debug, Clone)]
pub struct AdmitFailure {
    pub peer: String,
    pub available: f64,
    pub requested: f64,
    pub active_sae_count: usize,
    pub capacity: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp() -> Arc<BufferPool> {
        Arc::new(BufferPool::new(1000))
    }

    fn rates(pairs: &[(&str, f64)]) -> Arc<Mutex<HashMap<String, f64>>> {
        let m: HashMap<String, f64> = pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        Arc::new(Mutex::new(m))
    }

    fn fill_buffer(pool: &Arc<BufferPool>, peer: &str, n: usize) {
        for i in 0..n {
            let key = crate::state::buffer::TransportKey::new(
                common::ids::KeyId::new(format!("k{i}")),
                vec![0xAB; 32],
            );
            pool.for_peer(peer)
                .enc(common::security::KeyGrade::Qkd)
                .try_push(key)
                .unwrap();
        }
    }

    #[test]
    fn single_sae_gets_full_bucket() {
        let pool = bp();
        let rates = rates(&[("dkms-22", 100.0)]);
        // obs_window=1s para que el floor (refill×1) no enmascare la
        // fairness por ocupación.
        let m = SaeBufferBuckets::new(pool.clone(), rates, 1.0, 1.0);
        fill_buffer(&pool, "dkms-22", 100);
        // 1 SAE: cap = max(100*1, 100/1, 1) = 100; refill = 100/s.
        let sae = SaeId::new("sae_aa");
        assert!(m.try_admit_many(&["dkms-22".into()], &sae, 50.0).is_ok());
        assert!(m.try_admit_many(&["dkms-22".into()], &sae, 50.0).is_ok());
    }

    #[test]
    fn two_saes_share_capacity_50_50() {
        let pool = bp();
        let rates = rates(&[("dkms-22", 100.0)]);
        let m = SaeBufferBuckets::new(pool.clone(), rates, 1.0, 1.0);
        fill_buffer(&pool, "dkms-22", 100);
        let sae_a = SaeId::new("sae_aa");
        let sae_b = SaeId::new("sae_bb");
        // Primer admit del A con N=1: cap = max(100, 100, 1) = 100.
        // Pide 60 → tokens 40.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae_a, 60.0).is_ok());
        // Primer admit del B con N=2: cap = max(50, 50, 1) = 50.
        // Bucket nuevo arranca lleno a 50; pedir 60 falla.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae_b, 60.0).is_err());
        // 40 sí cabe.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae_b, 40.0).is_ok());
    }

    #[test]
    fn refund_restores_tokens() {
        let pool = bp();
        let rates = rates(&[("dkms-22", 50.0)]);
        let m = SaeBufferBuckets::new(pool.clone(), rates, 1.0, 1.0);
        fill_buffer(&pool, "dkms-22", 50);
        let sae = SaeId::new("sae_aa");
        // Primer admit con N=1: cap = max(50, 50, 1) = 50. Consume 50 → tokens 0.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae, 50.0).is_ok());
        // Sin refund: bucket vacío → falla.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae, 50.0).is_err());
        m.refund("dkms-22", &sae, 50.0);
        // Tras refund: 50 tokens. Vuelve a pasar.
        assert!(m.try_admit_many(&["dkms-22".into()], &sae, 50.0).is_ok());
    }
}
