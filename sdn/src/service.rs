use std::{sync::Arc, time::Duration};

use common::metrics::Metrics;
use tracing::{info, warn};

use crate::{
    config::SdnConfig,
    error::Result,
    push::Pushers,
    topology::{Topology, TopologyStore},
};

#[derive(Clone)]
pub struct SdnService {
    pub cfg:      Arc<SdnConfig>,
    pub topology: TopologyStore,
    pub pushers:  Arc<Pushers>,
    pub metrics:  Metrics,
}

impl SdnService {
    pub async fn new(cfg: SdnConfig, metrics: Metrics) -> Result<Self> {
        let initial = match &cfg.topology_file {
            Some(p) => match Topology::from_json(p) {
                Ok(t) => t,
                Err(e) => {
                    warn!(path = %p, error = %e, "failed to load initial topology, starting empty");
                    Topology::default()
                }
            },
            None => Topology::default(),
        };
        Ok(Self {
            cfg:      Arc::new(cfg),
            topology: TopologyStore::new(initial),
            pushers:  Arc::new(Pushers::new()),
            metrics,
        })
    }

    /// Periodic MCF recompute + debounced push. Runs forever.
    pub async fn run_background_tasks(self) -> Result<()> {
        info!("sdn: background tasks started");
        let mut tick = tokio::time::interval(Duration::from_millis(self.cfg.mcf_period_ms));
        loop {
            tick.tick().await;
            // TODO: collect demands, call mcf::recompute
            // TODO: if anything changed, broadcast via pushers
        }
    }
}
