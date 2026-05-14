use std::sync::Arc;

use common::metrics::Metrics;
use tracing::info;

use crate::{
    config::OrrConfig,
    error::Result,
    peers::PeerRegistry,
    relay::CircuitTable,
};

#[derive(Clone)]
pub struct OrrService {
    pub cfg:      Arc<OrrConfig>,
    pub circuits: Arc<CircuitTable>,
    pub peers:    Arc<PeerRegistry>,
    pub metrics:  Metrics,
}

impl OrrService {
    pub async fn new(cfg: OrrConfig, metrics: Metrics) -> Result<Self> {
        let cfg = Arc::new(cfg);
        info!(node = %cfg.node_id, "orr service initialized");
        Ok(Self {
            circuits: Arc::new(CircuitTable::new()),
            peers:    Arc::new(PeerRegistry::new(cfg.peers.clone())),
            cfg,
            metrics,
        })
    }
}
