use std::{sync::Arc, time::Duration};

use common::metrics::Metrics;
use tracing::info;

use crate::{
    buffer::BufferRegistry,
    config::DkmsConfig,
    error::Result,
    orr_client::OrrClient,
    qkc_client::QkcClient,
    scheduler::Scheduler,
    sdn_client::SdnClient,
    token_bucket::SaeBuckets,
};

#[derive(Clone)]
pub struct DkmsService {
    pub cfg:        Arc<DkmsConfig>,
    pub buffers:    Arc<BufferRegistry>,
    pub buckets:    Arc<SaeBuckets>,
    pub scheduler:  Arc<Scheduler>,
    pub sdn:        Arc<SdnClient>,
    pub orr:        Arc<OrrClient>,
    pub qkc:        Arc<QkcClient>,
    pub metrics:    Metrics,
}

impl DkmsService {
    pub async fn new(cfg: DkmsConfig, metrics: Metrics) -> Result<Self> {
        let cfg = Arc::new(cfg);
        Ok(Self {
            buffers:   Arc::new(BufferRegistry::new()),
            buckets:   Arc::new(SaeBuckets::new(cfg.default_sae_rate_keys_per_sec)),
            scheduler: Arc::new(Scheduler::new(cfg.scheduler_period_ms)),
            sdn:       Arc::new(SdnClient::new(cfg.sdn_url.clone())),
            orr:       Arc::new(OrrClient::new(cfg.orr_url.clone())),
            qkc:       Arc::new(QkcClient::new(cfg.qkc_url.clone())),
            cfg,
            metrics,
        })
    }

    /// Background work: round-robin scheduler + metric pushes to SDN.
    pub async fn run_background_tasks(self) -> Result<()> {
        info!("dkms: background tasks started");
        let mut tick = tokio::time::interval(Duration::from_millis(self.cfg.scheduler_period_ms));
        loop {
            tick.tick().await;
            // TODO: drain ready (sae_local, sae_remote) pairs via scheduler.
        }
    }
}
