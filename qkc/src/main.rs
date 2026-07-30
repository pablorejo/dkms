//! QKC binary entry point.
//!
//! Tres listeners + el `PeerOut` (pool de envío TCP).
//!
//! ```bash
//!   qkc --config qkc.toml
//! ```

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use qkc::{
    config::QkcConfig,
    http_admin,
    sdn_client::SdnAnnouncer,
    service::QkcService,
    transport::{local, peer_server},
};
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "qkc",
    version,
    about = "Quantum Key Channel — hop-by-hop OTP relay"
)]
struct Cli {
    /// Ruta al fichero TOML de configuración.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = QkcConfig::load(&cli.config)?;
    info!(?cfg, "qkc starting");

    let svc = QkcService::new(cfg.clone())?;
    // Workers de cada KeyStore (refill ENC en background, dispatcher
    // DEC al recibir FRAME_KEY_IDS_NOTIFY) + logger periódico.
    svc.bootstrap_keystores();

    let peer = tokio::spawn({
        let svc = svc.clone();
        let addr = cfg.peer_listen.clone();
        async move { peer_server::serve(svc, &addr).await }
    });
    let local_t = tokio::spawn({
        let svc = svc.clone();
        let addr = cfg.local_listen.clone();
        async move { local::serve(svc, &addr).await }
    });
    let admin = tokio::spawn({
        let svc = svc.clone();
        let addr = cfg.admin_http.clone();
        async move { http_admin::serve(svc, &addr).await }
    });

    // Anuncio periódico a la SDN para que nos incluya en su topología. Va
    // aparte del `select!` de abajo a propósito: si no hay `sdn_url`, o la SDN
    // está caída, el QKC sigue relayando claves igual.
    if let Some(announcer) = SdnAnnouncer::from_config(&cfg) {
        tokio::spawn(announcer.run());
    }

    tokio::select! {
        r = peer    => r??,
        r = local_t => r??,
        r = admin   => r??,
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
