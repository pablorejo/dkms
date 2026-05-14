//! `QkcService` — the cheap-to-clone handle that both the gRPC server and the
//! TCP server share. Wraps all the long-lived state behind `Arc`s so handlers
//! can grab it without locking.

use std::sync::Arc;

use common::metrics::Metrics;
use tracing::{debug, info};

use crate::{
    config::QkcConfig,
    crypto_engine::CryptoEngine,
    error::Result,
    kme::Kme,
    routing::RoutingResolver,
    token_bucket::TokenBucketRegistry,
};

#[derive(Clone)]
pub struct QkcService {
    pub cfg:     Arc<QkcConfig>,
    pub kme:     Arc<Kme>,
    pub crypto:  Arc<CryptoEngine>,
    pub routing: Arc<RoutingResolver>,
    pub buckets: Arc<TokenBucketRegistry>,
    pub metrics: Metrics,
}

impl QkcService {
    pub async fn new(cfg: QkcConfig, metrics: Metrics) -> Result<Self> {
        let cfg = Arc::new(cfg);
        Ok(Self {
            kme:     Arc::new(Kme::new(cfg.clone()).await?),
            crypto:  Arc::new(CryptoEngine::new()),
            routing: Arc::new(RoutingResolver::new(cfg.clone())),
            buckets: Arc::new(TokenBucketRegistry::new(cfg.default_refill_rate)),
            metrics,
            cfg,
        })
    }

    /// Long-running tasks: key replenishment from quditto, periodic capacity
    /// reports to SDN, token bucket refill, etc.
    pub async fn run_background_tasks(self) -> Result<()> {
        info!("qkc: background tasks started");

        // TODO: spawn each as a separate task; for the skeleton we just park.
        let _kme = self.kme.clone();
        let _buckets = self.buckets.clone();

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            debug!("qkc: background heartbeat");
        }
    }
}
