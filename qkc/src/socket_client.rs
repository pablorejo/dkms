//! Long-lived binary-TCP client used to push frames to peer QKCs.
//!
//! Maintains one connection per peer and queues frames behind a small
//! mpsc — keeps reconnection logic out of the hot path. Pulls peer
//! endpoints from `QkcConfig::peer_tcp`.

use std::{collections::HashMap, sync::Arc};

use common::ipc::binary_tcp::{write_frame, Frame};
use parking_lot::Mutex;
use tokio::{net::TcpStream, sync::mpsc};
use tracing::{debug, warn};

use crate::error::{QkcError, Result};

pub struct SocketClient {
    senders: Mutex<HashMap<String, mpsc::Sender<Frame>>>,
    peer_addrs: Arc<HashMap<String, String>>,
}

impl SocketClient {
    pub fn new(peer_addrs: Arc<HashMap<String, String>>) -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            peer_addrs,
        }
    }

    pub async fn send(&self, peer: &str, frame: Frame) -> Result<()> {
        let sender = self.ensure_sender(peer)?;
        sender
            .send(frame)
            .await
            .map_err(|e| QkcError::Transport(format!("send to {peer}: {e}")))
    }

    fn ensure_sender(&self, peer: &str) -> Result<mpsc::Sender<Frame>> {
        if let Some(s) = self.senders.lock().get(peer).cloned() {
            return Ok(s);
        }
        let addr = self
            .peer_addrs
            .get(peer)
            .ok_or_else(|| QkcError::Transport(format!("no addr for peer {peer}")))?
            .clone();

        let (tx, mut rx) = mpsc::channel::<Frame>(256);
        self.senders.lock().insert(peer.to_owned(), tx.clone());

        let peer_lbl = peer.to_owned();
        tokio::spawn(async move {
            loop {
                match TcpStream::connect(&addr).await {
                    Ok(mut s) => {
                        let _ = s.set_nodelay(true);
                        while let Some(frame) = rx.recv().await {
                            if let Err(e) = write_frame(&mut s, &frame).await {
                                warn!(peer = %peer_lbl, error = %e, "qkc tcp write failed, reconnecting");
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        debug!(peer = %peer_lbl, error = %e, "qkc tcp connect failed, retrying");
                        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                    }
                }
            }
        });

        Ok(tx)
    }
}
