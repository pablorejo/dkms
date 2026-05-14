//! DKMS binary entry point.
//!
//! Three listeners + a background scheduler:
//!   1. ETSI HTTP (axum, mTLS optional)   — SAEs face this.
//!   2. gRPC control plane (tonic)         — orchestrator faces this.
//!   3. Metrics HTTP                       — Prometheus scrape target.
//!   4. Background scheduler               — round-robin buffered delivery.

use anyhow::Result;
use clap::Parser;
use common::{logging, metrics::Metrics};
use dkms::{config::DkmsConfig, service::DkmsService};
use tokio::signal;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "dkms", version, about = "DKMS module")]
struct Cli {
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("dkms");
    let _cli = Cli::parse();

    let cfg: DkmsConfig = common::config::load_config("dkms")?;
    info!(?cfg, "dkms starting");

    let metrics = Metrics::new("dkms");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = DkmsService::new(cfg.clone(), metrics).await?;

    let http = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.http_addr.clone();
        async move { dkms::http_server::serve(svc, &addr).await }
    });
    let grpc = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.grpc_addr.clone();
        async move { dkms::grpc_server::serve(svc, &addr).await }
    });
    let bg = tokio::spawn({
        let svc = service.clone();
        async move { svc.run_background_tasks().await }
    });

    tokio::select! {
        r = http => r??,
        r = grpc => r??,
        r = bg   => r??,
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
