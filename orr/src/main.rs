//! ORR binary entry point.

use anyhow::Result;
use clap::Parser;
use common::{logging, metrics::Metrics};
use orr::{config::OrrConfig, service::OrrService};
use tokio::signal;
use tracing::info;

#[derive(Parser, Debug)]
#[command(name = "orr", version, about = "Onion Routing Router module")]
struct Cli {
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("orr");
    let _cli = Cli::parse();

    let cfg: OrrConfig = common::config::load_config("orr")?;
    info!(?cfg, "orr starting");

    let metrics = Metrics::new("orr");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = OrrService::new(cfg.clone(), metrics).await?;

    let grpc = tokio::spawn({
        let svc = service.clone();
        async move { orr::grpc_server::serve(svc, &cfg.grpc_addr).await }
    });

    tokio::select! {
        r = grpc => { r??; }
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
