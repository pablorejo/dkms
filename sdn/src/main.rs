//! SDN binary entry point.
//!
//! Spawns three tasks:
//!   1. gRPC server — control plane towards DKMS/QKC/ORR.
//!   2. HTTP server — admin API (mostly read-only) consumed by the web UI.
//!   3. Background — periodic MCF recompute + debounced topology pushes.

use anyhow::Result;
use clap::Parser;
use common::{logging, metrics::Metrics};
use sdn::{config::SdnConfig, service::SdnService};
use tokio::signal;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "sdn", version, about = "Software Defined Network module")]
struct Cli {
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("sdn");
    let _cli = Cli::parse();

    let cfg: SdnConfig = common::config::load_config("sdn")?;
    info!(?cfg, "sdn starting");

    let metrics = Metrics::new("sdn");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = SdnService::new(cfg.clone(), metrics).await?;

    let grpc = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.grpc_addr.clone();
        async move { sdn::grpc_server::serve(svc, &addr).await }
    });
    let http = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.http_addr.clone();
        async move { sdn::http_api::serve(svc, &addr).await }
    });
    let bg = tokio::spawn({
        let svc = service.clone();
        async move { svc.run_background_tasks().await }
    });

    tokio::select! {
        r = grpc => r??,
        r = http => r??,
        r = bg   => r??,
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
