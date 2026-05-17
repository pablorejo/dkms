//! Token bucket por SAE.
//!
//! Coste de una petición ETSI 014 (`enc_keys`):
//!
//! ```text
//!     tokens = ceil(size_bytes / token_unit_bytes)
//!            * number
//!            * count(distinct destination DKMSs)
//! ```
//!
//! Soporta **reembolso** porque la política de fallo (un destino DKMS
//! caído ⇒ falla la petición entera) requiere devolver los tokens cuando
//! la entrega no termina con éxito.
//!
//! El bucket es *lock-light* (un `parking_lot::Mutex` por SAE; el mapa de
//! SAEs es un `DashMap`). En el camino caliente solo se toca el `Mutex` del
//! SAE concreto, no el `DashMap` global.

use std::time::Instant;

use dashmap::DashMap;
use parking_lot::Mutex;

use common::ids::SaeId;

/// Parámetros que cuesta calcular para una petición concreta.
pub fn compute_cost(
    size_bytes: u64,
    number: u64,
    num_destination_dkms: u64,
    token_unit_bytes: u32,
) -> u64 {
    let unit = token_unit_bytes.max(1) as u64;
    let per_key = size_bytes.div_ceil(unit);
    per_key
        .saturating_mul(number)
        .saturating_mul(num_destination_dkms.max(1))
}

struct State {
    tokens: u64,
    last: Instant,
}

/// Bucket de un SAE concreto.
pub struct Bucket {
    capacity: u64,
    refill_per_sec: u64,
    state: Mutex<State>,
}

impl Bucket {
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            state: Mutex::new(State {
                tokens: capacity,
                last: Instant::now(),
            }),
        }
    }

    fn refill_inplace(&self, s: &mut State, now: Instant) {
        let elapsed = now.saturating_duration_since(s.last).as_secs_f64();
        let refilled = (elapsed * self.refill_per_sec as f64) as u64;
        if refilled > 0 {
            s.tokens = s.tokens.saturating_add(refilled).min(self.capacity);
            s.last = now;
        }
    }

    /// Intenta consumir `n` tokens; si no hay suficientes, devuelve cuántos
    /// hay disponibles tras refrescar el bucket (útil para el error
    /// `RateLimited` que devuelve esta cifra al cliente).
    pub fn try_consume(&self, n: u64) -> std::result::Result<(), u64> {
        let mut s = self.state.lock();
        let now = Instant::now();
        self.refill_inplace(&mut s, now);
        if s.tokens >= n {
            s.tokens -= n;
            Ok(())
        } else {
            Err(s.tokens)
        }
    }

    /// Devuelve `n` tokens al bucket, sin sobrepasar `capacity`.
    pub fn refund(&self, n: u64) {
        let mut s = self.state.lock();
        s.tokens = s.tokens.saturating_add(n).min(self.capacity);
    }

    pub fn snapshot(&self) -> (u64, u64) {
        let s = self.state.lock();
        (s.tokens, self.capacity)
    }
}

/// Mapa SAE → bucket, con creación perezosa por valores por defecto.
pub struct SaeBuckets {
    default_capacity: u64,
    default_refill: u64,
    map: DashMap<SaeId, Bucket>,
}

impl SaeBuckets {
    pub fn new(default_refill: u64, default_capacity: u64) -> Self {
        Self {
            default_capacity,
            default_refill,
            map: DashMap::new(),
        }
    }

    fn ensure(&self, sae: &SaeId) {
        // entry().or_insert_with evita doble inserción.
        if !self.map.contains_key(sae) {
            self.map
                .entry(sae.clone())
                .or_insert_with(|| Bucket::new(self.default_capacity, self.default_refill));
        }
    }

    pub fn set_limits(&self, sae: &SaeId, refill_per_sec: u64, capacity: u64) {
        self.map
            .insert(sae.clone(), Bucket::new(capacity, refill_per_sec));
    }

    /// Intenta consumir `n` tokens al SAE. Si no se ha visto antes se le
    /// asignan los valores por defecto.
    pub fn try_consume(&self, sae: &SaeId, n: u64) -> std::result::Result<(), u64> {
        self.ensure(sae);
        let b = self.map.get(sae).expect("just ensured");
        b.try_consume(n)
    }

    /// Reembolsa `n` tokens al SAE.
    pub fn refund(&self, sae: &SaeId, n: u64) {
        if let Some(b) = self.map.get(sae) {
            b.refund(n);
        }
    }

    pub fn snapshot(&self, sae: &SaeId) -> Option<(u64, u64)> {
        self.map.get(sae).map(|b| b.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_uses_ceil_div_per_dkms_and_per_key() {
        // 33 bytes → 2 unidades de 32; 3 claves; 4 DKMSs destino.
        assert_eq!(compute_cost(33, 3, 4, 32), 2 * 3 * 4);
        // 32 bytes redondos.
        assert_eq!(compute_cost(32, 1, 1, 32), 1);
        // 0 DKMS destino se trata como 1 (SAE local).
        assert_eq!(compute_cost(32, 1, 0, 32), 1);
    }

    #[test]
    fn refund_restores_tokens_capped_at_capacity() {
        let b = Bucket::new(10, 0); // sin refill automático
        b.try_consume(7).unwrap();
        b.refund(100);
        assert_eq!(b.snapshot().0, 10);
    }

    #[test]
    fn try_consume_returns_available_when_short() {
        let b = Bucket::new(5, 0);
        b.try_consume(3).unwrap();
        let err = b.try_consume(10).unwrap_err();
        assert_eq!(err, 2);
    }
}
