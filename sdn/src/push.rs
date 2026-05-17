//! Pushes `TopologyEvent`s to subscribers (DKMS, QKC, ORR).
//!
//! Each subscriber is a `mpsc::Sender<TopologyEvent>`; the streaming RPC
//! handler in `grpc_server` registers one per active subscription.

use common::proto::sdn::v1::TopologyEvent;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::debug;

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
        self.subscribers.lock().push(tx);
        rx
    }

    pub async fn broadcast(&self, ev: TopologyEvent) {
        let mut to_remove = vec![];
        let subs: Vec<_> = self.subscribers.lock().clone();
        for (i, tx) in subs.iter().enumerate() {
            if tx.send(Ok(ev.clone())).await.is_err() {
                to_remove.push(i);
            }
        }
        if !to_remove.is_empty() {
            let mut g = self.subscribers.lock();
            to_remove.into_iter().rev().for_each(|i| {
                g.remove(i);
            });
            debug!(remaining = g.len(), "pruned closed topology subscribers");
        }
    }
}

impl Default for Pushers {
    fn default() -> Self {
        Self::new()
    }
}
