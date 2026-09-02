//! Pushes `TopologyEvent`s to subscribers (DKMS, QKC, ORR).
//!
//! Each subscriber is a `mpsc::Sender<TopologyEvent>`; the streaming RPC
//! handler in `grpc_server` registers one per active subscription.

use common::proto::sdn::v1::TopologyEvent;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;

/// Tope de suscriptores vivos a `StreamTopology` (B10).
const MAX_SUBSCRIBERS: usize = 256;

pub struct Pushers {
    subscribers: Mutex<Vec<mpsc::Sender<std::result::Result<TopologyEvent, tonic::Status>>>>,
}

impl Pushers {
    pub fn new() -> Self {
        Self {
            subscribers: Mutex::new(Vec::new()),
        }
    }

    pub fn subscribe(&self) -> mpsc::Receiver<std::result::Result<TopologyEvent, tonic::Status>> {
        let (tx, rx) = mpsc::channel(64);
        let mut g = self.subscribers.lock();
        // Poda los cerrados y acota el número de suscriptores (B10): sin cota,
        // cualquiera (o cualquier miembro bajo mTLS) abría streams sin límite.
        g.retain(|s| !s.is_closed());
        if g.len() >= MAX_SUBSCRIBERS {
            g.remove(0); // suelta el más viejo
        }
        g.push(tx);
        rx
    }

    pub async fn broadcast(&self, ev: TopologyEvent) {
        // `try_send` bajo el lock, SIN await (parking_lot): un suscriptor que no
        // lee ya no puede bloquear el push de forwarding a todos los QKC — antes
        // `send().await` sobre un canal lleno colgaba la MISMA task que empuja
        // las tablas (auditoría 2026-09b B10). Un canal lleno o cerrado se
        // desconecta (reconecta y re-sincroniza). `retain` evita además el bug
        // de índices obsoletos del clon anterior.
        let mut g = self.subscribers.lock();
        let before = g.len();
        g.retain(|tx| tx.try_send(Ok(ev.clone())).is_ok());
        if g.len() != before {
            debug!(
                remaining = g.len(),
                "pruned lagging/closed topology subscribers"
            );
        }
    }

    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len()
    }
}

impl Default for Pushers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_subscriber_does_not_block_broadcast_and_is_pruned() {
        let p = Pushers::new();
        let _rx = p.subscribe(); // nunca lee: su canal (64) se llena
        let ev = TopologyEvent::default();
        // Muchos más broadcasts que la capacidad del canal: ninguno debe
        // bloquear, y el suscriptor lleno se poda.
        for _ in 0..70 {
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                p.broadcast(ev.clone()),
            )
            .await
            .expect("broadcast no debe bloquear con un suscriptor lleno");
        }
        assert_eq!(p.subscriber_count(), 0, "el suscriptor lleno debe podarse");
    }
}
