//! QKC binary entry point.
//!
//! Loads config, sets up tracing + metrics, and spawns:
//!   1. The gRPC control-plane server (`qkc::grpc_server`).
//!   2. The binary-TCP hot-path server (`qkc::socket_server`).
//!   3. The KME background tasks (key replenishment, token bucket refill).
//!
//! Each task runs to the first error or shutdown signal.

use anyhow::Result;
use clap::Parser;
use common::{logging, metrics::Metrics};
use qkc::{config::QkcConfig, service::QkcService};
use tokio::signal;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "qkc", version, about = "Quantum Key Channel module")]
struct Cli {
    /// Override the config directory (otherwise reads `CONFIG_DIR` env, then `./config`).
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("qkc");

    let _cli = Cli::parse();
    let cfg: QkcConfig = common::config::load_config("qkc")?;
    info!(?cfg, "qkc starting");

    let metrics = Metrics::new("qkc");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = QkcService::new(cfg.clone(), metrics.clone()).await?;

    let grpc = tokio::spawn({
        let svc = service.clone();
        async move { qkc::grpc_server::serve(svc, &cfg.grpc_addr).await }
    });

    let tcp = tokio::spawn({
        let svc = service.clone();
        let bind = cfg.tcp_bind.clone();
        async move { qkc::socket_server::serve(svc, bind).await }
    });

    let bg = tokio::spawn({
        let svc = service.clone();
        async move { svc.run_background_tasks().await }
    });

    tokio::select! {
        r = grpc => { r??; }
        r = tcp  => { r??; }
        r = bg   => { r??; }
        _ = signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
    }

    Ok(())
}
