//! Estado de admisión para apagado controlado (`Drain`).
//!
//! Tres piezas:
//!
//! * `accepting` — `AtomicBool` que los planos HTTP miran al recibir cada
//!   request. Mientras esté `true`, se admite; al hacer `close()` cualquier
//!   nueva request se rechaza con 503.
//! * `inflight` — `AtomicU64` con el número de requests aceptadas que
//!   siguen en curso.
//! * `drained` — `tokio::sync::Notify` que se dispara cuando `inflight`
//!   baja a 0. `wait_drained()` espera ese punto.
//!
//! Para que `inflight` sea correcto incluso si el handler entra en pánico,
//! cada request entra en un [`InflightGuard`] RAII que decrementa al hacer
//! drop.
//!
//! Diseño de orden de memoria: `Acquire`/`Release` para que el observador
//! que ve `inflight == 0` también vea efectos `accepting=false` previos.

use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

use tokio::sync::Notify;

pub struct Admission {
    accepting: AtomicBool,
    inflight: AtomicU64,
    drained: Notify,
}

impl Admission {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            accepting: AtomicBool::new(true),
            inflight: AtomicU64::new(0),
            drained: Notify::new(),
        })
    }

    pub fn is_accepting(&self) -> bool {
        self.accepting.load(Ordering::Acquire)
    }

    /// Detiene la admisión. Idempotente.
    pub fn close(&self) {
        self.accepting.store(false, Ordering::Release);
    }

    /// Reabre la admisión (utilidad de test/admin; en producción
    /// normalmente el proceso muere tras `drain`).
    pub fn reopen(&self) {
        self.accepting.store(true, Ordering::Release);
    }

    /// Reserva una unidad de "request in-flight". El handler debe quedarse
    /// con el guard hasta que termine; al hacer drop se decrementa.
    pub fn acquire(self: &Arc<Self>) -> InflightGuard {
        self.inflight.fetch_add(1, Ordering::AcqRel);
        InflightGuard {
            inner: self.clone(),
        }
    }

    pub fn inflight(&self) -> u64 {
        self.inflight.load(Ordering::Acquire)
    }

    /// Espera hasta que `inflight == 0`. Si ya está a cero, retorna sin
    /// suspenderse.
    pub async fn wait_drained(&self) {
        loop {
            if self.inflight.load(Ordering::Acquire) == 0 {
                return;
            }
            // Suscribirse ANTES de re-checkear evita race: si el último
            // request hace fetch_sub justo entre el check y el await,
            // `notified()` queda registrada y se despierta enseguida.
            let waiter = self.drained.notified();
            if self.inflight.load(Ordering::Acquire) == 0 {
                return;
            }
            waiter.await;
        }
    }
}

pub struct InflightGuard {
    inner: Arc<Admission>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        // fetch_sub devuelve el valor previo; si era 1 ahora es 0.
        if self.inner.inflight.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.inner.drained.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn wait_drained_returns_immediately_when_zero() {
        let a = Admission::new();
        tokio::time::timeout(Duration::from_millis(50), a.wait_drained())
            .await
            .expect("must return without timing out");
    }

    #[tokio::test]
    async fn wait_drained_resolves_when_last_guard_drops() {
        let a = Admission::new();
        let g1 = a.acquire();
        let g2 = a.acquire();
        assert_eq!(a.inflight(), 2);

        let waiter = {
            let a = a.clone();
            tokio::spawn(async move { a.wait_drained().await })
        };
        // dale tiempo al waiter a registrarse
        tokio::time::sleep(Duration::from_millis(10)).await;
        drop(g1);
        drop(g2);
        tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("waiter must finish")
            .unwrap();
        assert_eq!(a.inflight(), 0);
    }

    #[tokio::test]
    async fn close_blocks_new_admissions_but_not_running() {
        let a = Admission::new();
        let g = a.acquire();
        a.close();
        assert!(!a.is_accepting());
        // el guard adquirido antes sigue siendo válido
        assert_eq!(a.inflight(), 1);
        drop(g);
        assert_eq!(a.inflight(), 0);
    }
}
