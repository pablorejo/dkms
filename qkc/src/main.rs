//! QKC binary entry point.
//!
//! Tres listeners (peer TCP, local TCP hacia el ORR, HTTP admin) + el
//! `PeerOut` (pool de envío TCP) + el bucle de anuncio a la SDN si hay
//! `sdn_url`. Toda la config viene del TOML de `--config`.
//!
//! ```bash
//!   qkc --config qkc.toml
//! ```

#![forbid(unsafe_code)]
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
    // Claves fuera de swap y de core dumps (best-effort, ver common::hardening).
    common::hardening::harden_process();

    // Proveedor rustls: necesario si el anuncio al SDN va por mTLS (https).
    common::tls_pqc::ensure_process_default().map_err(anyhow::Error::msg)?;

    let cli = Cli::parse();
    let cfg = QkcConfig::load(&cli.config)?;
    info!(?cfg, "qkc starting");
    // Self-check del intercambio de claves TLS (híbrido post-cuántico) con la
    // identidad del nodo, si la hay; sin identidad no hay TLS que comprobar.
    if let Some(t) = &cfg.tls {
        common::tls_pqc::self_check_hybrid_kx_files(&t.cert_path, &t.key_path)
            .map_err(anyhow::Error::msg)?;
    }

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
        // Con [tls] el admin (push de forwarding) va con mTLS obligatorio.
        let tls = cfg.tls.clone();
        async move { http_admin::serve(svc, &addr, tls).await }
    });

    // Anuncio periódico a la SDN para que nos incluya en su topología. Va
    // aparte del `select!` de abajo a propósito: si no hay `sdn_url`, o la SDN
    // está caída, el QKC sigue relayando claves igual.
    if let Some(announcer) = SdnAnnouncer::from_config(&cfg, svc.clone()) {
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
