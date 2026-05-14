//! Coalesces many small topology mutations into a single push event.
//!
//! Drop events into the input; after a quiet window of `window_ms`, the
//! output emits one merged push.

use std::time::Duration;

use tokio::sync::mpsc;
use tracing::trace;

pub struct Debouncer {
    window: Duration,
    tx_in:  mpsc::Sender<()>,
}

impl Debouncer {
    /// Creates a debouncer that calls `on_fire` once after the input goes
    /// quiet for `window_ms`. The returned `Debouncer` is the input.
    pub fn new<F>(window_ms: u64, mut on_fire: F) -> Self
    where
        F: FnMut() + Send + 'static,
    {
        let (tx_in, mut rx_in) = mpsc::channel::<()>(256);
        let window = Duration::from_millis(window_ms);
        tokio::spawn(async move {
            let mut pending = false;
            loop {
                tokio::select! {
                    Some(_) = rx_in.recv() => {
                        pending = true;
                        trace!("debounce: tick");
                    }
                    _ = tokio::time::sleep(window), if pending => {
                        on_fire();
                        pending = false;
                    }
                }
            }
        });
        Self { window, tx_in }
    }

    pub async fn tick(&self) {
        let _ = self.tx_in.send(()).await;
    }

    pub fn window(&self) -> Duration { self.window }
}
