//! Quién sigue vivo, para poder olvidar a quien no.
//!
//! Los módulos que se auto-registran reanuncian periódicamente; ese reanuncio
//! es su heartbeat. Aquí se apunta cuándo se oyó a cada uno por última vez, y
//! el barredor de [`crate::service`] saca de la topología a los que llevan más
//! de un TTL callados. Sin esto un nodo apagado se queda en el grafo para
//! siempre y el solver le sigue asignando caudal que nadie consume.
//!
//! **Vive fuera de [`crate::topology::Topology`] a propósito.** El snapshot se
//! clona y se compara entero para decidir si hubo cambio, así que un timestamp
//! dentro haría que cada heartbeat pareciese una modificación: bump de versión,
//! push de forwarding y LP nuevo cada pocos segundos por nodo. Justo lo que el
//! diseño idempotente del registro evita.
//!
//! **Solo caduca lo que se anunció.** Una entidad cargada de ficheros al
//! arrancar nunca entra aquí, así que nunca expira: la declaró el operador, no
//! se está declarando ella. Mezclar ambos sin esta distinción borraría
//! topología estática a los 90 s de arrancar.

use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Qkc,
    Orr,
    Dkms,
}

impl Kind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Kind::Qkc => "qkc",
            Kind::Orr => "orr",
            Kind::Dkms => "dkms",
        }
    }
}

/// Último anuncio oído de cada entidad auto-registrada.
#[derive(Debug, Default)]
pub struct Presence {
    seen: DashMap<(Kind, String), Instant>,
}

impl Presence {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registra que acabamos de oír a `id`. Idempotente y barato.
    pub fn touch(&self, kind: Kind, id: &str) {
        self.seen.insert((kind, id.to_string()), Instant::now());
    }

    /// Deja de vigilar a `id` (ya lo hemos echado, o se borró a mano).
    pub fn forget(&self, kind: Kind, id: &str) {
        self.seen.remove(&(kind, id.to_string()));
    }

    /// Entidades que llevan más de `ttl` sin anunciarse. **Las saca del
    /// registro al devolverlas**, para no reintentar el borrado en cada
    /// barrido si la topología ya no las tiene.
    pub fn take_expired(&self, ttl: Duration) -> Vec<(Kind, String)> {
        let now = Instant::now();
        let stale: Vec<_> = self
            .seen
            .iter()
            .filter(|r| now.duration_since(*r.value()) > ttl)
            .map(|r| r.key().clone())
            .collect();
        for k in &stale {
            self.seen.remove(k);
        }
        stale
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_expires_before_its_ttl() {
        let p = Presence::new();
        p.touch(Kind::Qkc, "1");
        assert!(p.take_expired(Duration::from_secs(60)).is_empty());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn expired_entries_come_out_once_and_only_once() {
        let p = Presence::new();
        p.touch(Kind::Qkc, "1");
        p.touch(Kind::Orr, "orr_1");

        // TTL cero ⇒ todo lo visto ya caducó.
        let got = p.take_expired(Duration::ZERO);
        assert_eq!(got.len(), 2);
        assert!(got.contains(&(Kind::Qkc, "1".to_string())));
        assert!(got.contains(&(Kind::Orr, "orr_1".to_string())));
        // Segundo barrido: ya no están, no se reintenta el borrado.
        assert!(p.take_expired(Duration::ZERO).is_empty());
        assert!(p.is_empty());
    }

    #[test]
    fn touching_again_resets_the_clock() {
        let p = Presence::new();
        p.touch(Kind::Dkms, "dkms-1");
        p.touch(Kind::Dkms, "dkms-1");
        assert_eq!(p.len(), 1, "el mismo id no se duplica");
        assert!(p.take_expired(Duration::from_secs(60)).is_empty());
    }

    #[test]
    fn forget_stops_watching() {
        let p = Presence::new();
        p.touch(Kind::Qkc, "1");
        p.forget(Kind::Qkc, "1");
        assert!(p.take_expired(Duration::ZERO).is_empty());
    }
}
