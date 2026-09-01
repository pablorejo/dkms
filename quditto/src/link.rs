//! Enlace QKD simulado.
//!
//! Mantiene dos estructuras:
//!
//! * `fresh` — FIFO lock-free de claves recién minteadas, sin entregar.
//!   Lo consume `enc_keys` y lo rellena el background generator.
//!   Implementado sobre [`crossbeam_queue::ArrayQueue`] — push/pop
//!   atómicos sin Mutex.
//! * `delivered` — mapa `key_id -> [u8; 32]` con las claves entregadas
//!   al lado master. `dec_keys?key_ID=...` lo consulta y lo consume
//!   (semántica one-shot). [`DashMap`] reparte la presión entre shards.
//!
//! Las stats están en `AtomicU64` para que el hot path no toque ningún
//! lock.
//!
//! **Rate model** (atenuación de fibra):
//!
//! ```text
//!   R(d) = R₀ · 10^(-α · d / 10)   keys/s
//! ```
//!
//! No depende del fill. El back-pressure se aplica descartando ticks
//! (no se acumula deuda histórica).

use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_queue::ArrayQueue;
use dashmap::DashMap;
use uuid::Uuid;

use crate::{config::QudittoConfig, crypto::Key};

/// `R(d) = r0 · 10^(-α·d/10)` en keys/s.
#[inline]
pub fn link_rate_kps(r0_kps: f64, alpha: f64, distance_km: f64) -> f64 {
    debug_assert!(r0_kps > 0.0);
    debug_assert!(alpha >= 0.0);
    debug_assert!(distance_km >= 0.0);
    r0_kps * 10f64.powf(-alpha * distance_km / 10.0)
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub generated: u64,
    pub dropped: u64,
    /// Claves NO destiladas por buffer lleno en modo `pause` (el análogo de
    /// `dropped` para el hardware real, que para en vez de tirar).
    pub paused: u64,
    pub delivered_enc: u64,
    pub delivered_dec: u64,
}

pub struct LinkBuffer {
    pub cfg: QudittoConfig,
    pub rate_kps: f64,

    fresh: ArrayQueue<Key>,
    delivered: DashMap<Uuid, zeroize::Zeroizing<Vec<u8>>>,

    // Stats: lock-free counters. Lectura coherente entre sí no
    // garantizada — solo aproximaciones para `/status`.
    n_generated: AtomicU64,
    n_dropped: AtomicU64,
    n_paused: AtomicU64,
    n_delivered_enc: AtomicU64,
    n_delivered_dec: AtomicU64,
}

impl LinkBuffer {
    pub fn new(cfg: QudittoConfig) -> Self {
        let rate_kps = link_rate_kps(cfg.r0, cfg.alpha, cfg.distance_km);
        Self {
            fresh: ArrayQueue::new(cfg.max_buffer_keys as usize),
            delivered: DashMap::with_capacity(cfg.max_buffer_keys as usize),
            cfg,
            rate_kps,
            n_generated: AtomicU64::new(0),
            n_dropped: AtomicU64::new(0),
            n_paused: AtomicU64::new(0),
            n_delivered_enc: AtomicU64::new(0),
            n_delivered_dec: AtomicU64::new(0),
        }
    }

    /// Tasa efectiva del enlace en claves/s — no depende del fill.
    pub fn current_rate_kps(&self) -> f64 {
        self.rate_kps
    }

    /// Empuja una clave al buffer fresh. Si está lleno se descarta
    /// (back-pressure por drop, sin acumular deuda).
    ///
    /// Devuelve `true` si la clave entró, `false` si se descartó.
    #[inline]
    pub fn push(&self, key: Key) -> bool {
        match self.fresh.push(key) {
            Ok(()) => {
                self.n_generated.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(_) => {
                self.n_dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    /// Saca hasta `count` claves frescas del FIFO y las mueve al map
    /// `delivered` para que `dec_keys` las pueda recuperar después.
    /// Devuelve las claves entregadas (puede ser menos de `count` si
    /// el buffer estaba más vacío).
    pub fn take_for_enc(&self, count: usize) -> Vec<Key> {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            match self.fresh.pop() {
                Some(k) => {
                    self.delivered.insert(k.key_id, k.material.clone());
                    out.push(k);
                }
                None => break,
            }
        }
        self.n_delivered_enc
            .fetch_add(out.len() as u64, Ordering::Relaxed);
        out
    }

    /// Recupera una clave previamente entregada por su `key_id`.
    /// La elimina del mapa para que un mismo `key_id` no se pueda
    /// usar dos veces (semántica OTP).
    pub fn take_for_dec(&self, key_id: &Uuid) -> Option<zeroize::Zeroizing<Vec<u8>>> {
        let result = self.delivered.remove(key_id).map(|(_, v)| v);
        if result.is_some() {
            self.n_delivered_dec.fetch_add(1, Ordering::Relaxed);
        }
        result
    }

    /// Número actual de claves disponibles para entregar por `enc_keys`.
    #[inline]
    pub fn fresh_available(&self) -> u64 {
        self.fresh.len() as u64
    }

    /// Hueco libre del FIFO `fresh` (para el modo `pause` del minter).
    #[inline]
    pub fn fresh_space(&self) -> u64 {
        (self.fresh.capacity() - self.fresh.len()) as u64
    }

    /// El minter en modo `pause` dejó de destilar `n` claves por falta de
    /// hueco.
    #[inline]
    pub fn note_paused(&self, n: u64) {
        self.n_paused.fetch_add(n, Ordering::Relaxed);
    }

    /// Número de `key_id`s entregados-pero-no-leídos-por-dec.
    #[inline]
    pub fn delivered_pending(&self) -> u64 {
        self.delivered.len() as u64
    }

    pub fn stats_snapshot(&self) -> Stats {
        Stats {
            generated: self.n_generated.load(Ordering::Relaxed),
            dropped: self.n_dropped.load(Ordering::Relaxed),
            paused: self.n_paused.load(Ordering::Relaxed),
            delivered_enc: self.n_delivered_enc.load(Ordering::Relaxed),
            delivered_dec: self.n_delivered_dec.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::{build_rng, Key};

    fn cfg() -> QudittoConfig {
        QudittoConfig {
            listen: "127.0.0.1:0".into(),
            r0: 1000.0,
            alpha: 0.2,
            distance_km: 5.0,
            max_buffer_keys: 4,
            key_size_bits: 256,
            full_mode: crate::config::FullMode::Drop,
            block_keys: 0,
            rate_step: None,
        }
    }

    #[test]
    fn rate_formula_matches_physical_attenuation() {
        // R(0) = R0
        assert!((link_rate_kps(1000.0, 0.2, 0.0) - 1000.0).abs() < 1e-9);
        // R(50 km, α=0.2 dB/km) = R0 · 10^(-1.0) = R0/10
        assert!((link_rate_kps(1000.0, 0.2, 50.0) - 100.0).abs() < 1e-9);
    }

    #[test]
    fn push_drops_when_full() {
        let lb = LinkBuffer::new(cfg());
        let mut r = build_rng();
        for _ in 0..4 {
            assert!(lb.push(Key::mint(&mut r, 32)));
        }
        // 5ª se descarta.
        assert!(!lb.push(Key::mint(&mut r, 32)));
        let s = lb.stats_snapshot();
        assert_eq!(s.generated, 4);
        assert_eq!(s.dropped, 1);
    }

    #[test]
    fn enc_moves_to_delivered_dec_consumes_it() {
        let lb = LinkBuffer::new(cfg());
        let mut r = build_rng();
        let k = Key::mint(&mut r, 32);
        let id = k.key_id;
        let mat = k.material.clone();
        lb.push(k);

        let out = lb.take_for_enc(1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].key_id, id);
        assert_eq!(lb.delivered_pending(), 1);

        let recovered = lb.take_for_dec(&id).expect("must be retrievable by id");
        assert_eq!(recovered, mat);

        // Segunda recuperación falla (one-shot).
        assert!(lb.take_for_dec(&id).is_none());
    }
}
