//! SDN binary entry point.
//!
//! Spawns three tasks:
//!   1. gRPC server — control plane towards DKMS/QKC/ORR.
//!   2. HTTP server — admin API (mostly read-only) consumed by the web UI.
//!   3. Background — periodic MCF recompute + debounced topology pushes.

#![forbid(unsafe_code)]
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
    // Claves fuera de swap y de core dumps (best-effort, ver common::hardening).
    common::hardening::harden_process();
    let _cli = Cli::parse();

    // Proveedor criptográfico de rustls: obligatorio antes de cualquier
    // handshake TLS (el plano de control mTLS opcional lo usa). Sin esto
    // rustls hace panic en el primer handshake. Idempotente si ya estaba.
    common::tls_pqc::ensure_process_default().map_err(anyhow::Error::msg)?;

    let cfg: SdnConfig = common::config::load_config("sdn")?;
    info!(?cfg, "sdn starting");
    // Self-check del intercambio de claves TLS (híbrido post-cuántico) con la
    // identidad del nodo, si la hay; sin identidad no hay TLS que comprobar.
    if let Some(t) = &cfg.tls {
        common::tls_pqc::self_check_hybrid_kx_files(&t.cert_path, &t.key_path)
            .map_err(anyhow::Error::msg)?;
    }

    let metrics = Metrics::new("sdn");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = SdnService::new(cfg.clone(), metrics).await?;

    let grpc = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.grpc_addr.clone();
        let tls = cfg.tls.clone();
        async move { sdn::grpc_server::serve(svc, &addr, tls.as_ref()).await }
    });
    let http = tokio::spawn({
        let svc = service.clone();
        let addr = cfg.http_addr.clone();
        let tls = cfg.tls.clone();
        let ro = cfg.http_ro_addr.clone();
        async move { sdn::http_api::serve_with_tls(svc, &addr, tls.as_ref(), ro.as_deref()).await }
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
