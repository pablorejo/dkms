//! Binario DKMS.
//!
//! Cuatro tareas concurrentes:
//!
//! * Plano norte (ETSI 014, SAE-facing, mTLS).
//! * Plano este/oeste (ETSI 020, DKMS↔DKMS, mTLS).
//! * Plano de gestión gRPC (`DkmsControl`).
//! * Endpoint Prometheus (`/metrics`).
//! * Tareas de fondo (sweeper de pending, refill de buffers — TODO QKC).

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::Parser;
use tokio::signal;
use tracing::info;

use common::{logging, metrics::Metrics};
use dkms::{
    config::DkmsConfig,
    etsi_http, grpc_server,
    peer_client::PeerHttpClient,
    sae_binding::{SaeBindingCache, StaticSaeResolver},
    service::DkmsService,
    state::{BufferPool, PendingStore},
    token_bucket::SaeBuckets,
};

#[derive(Parser, Debug)]
#[command(name = "dkms", version, about = "DKMS (ETSI 014/020 key delivery)")]
struct Cli {
    /// Directorio de configuración (sobrescribe `CONFIG_DIR`).
    #[arg(long, env = "CONFIG_DIR")]
    config_dir: Option<String>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    logging::init("dkms");
    let cli = Cli::parse();
    if let Some(d) = cli.config_dir.as_deref() {
        std::env::set_var("CONFIG_DIR", d);
    }

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok(); // idempotente; ignora si ya estaba puesto

    let cfg: DkmsConfig = common::config::load_config("dkms")?;
    info!(node_id = %cfg.node_id, "dkms starting");

    // ─── Métricas ─────────────────────────────────────────────────────
    let metrics = Metrics::new("dkms");
    metrics
        .serve(cfg.listen.metrics_addr.to_string())
        .await
        .context("metrics listener")?;

    // ─── Estado ───────────────────────────────────────────────────────
    let cfg = Arc::new(cfg);
    let pool = Arc::new(BufferPool::new(cfg.buffer.capacity_per_peer));
    let pending = Arc::new(PendingStore::new(cfg.pending.default_ttl_secs));
    let buckets = Arc::new(SaeBuckets::new(
        cfg.sae.default_rate_keys_per_sec,
        cfg.sae.default_burst_keys,
    ));

    // ─── SAE binding resolver ─────────────────────────────────────────
    // Por ahora resolver estático: identidad (un SAE vive en este DKMS).
    // Cuando SDN exponga su RPC, se cambia a un `SdnSaeResolver`.
    let static_resolver = Arc::new(StaticSaeResolver::new());
    let sae_binding = Arc::new(SaeBindingCache::new(
        static_resolver,
        cfg.sae_binding.ttl_secs,
        cfg.sae_binding.max_entries,
    ));

    // ─── Cliente DKMS↔DKMS ────────────────────────────────────────────
    let peer_client = Arc::new(
        PeerHttpClient::build(
            &cfg.tls.cert_path,
            &cfg.tls.key_path,
            &cfg.tls.peer_dkms_ca,
            cfg.request.clone(),
        )
        .context("peer http client")?,
    );

    // ─── Clientes sur (SDN/QKC) ───────────────────────────────────────
    // En esta fase del rewrite arrancan en None (la SDN y QKC están en
    // flujo). El servicio funciona sin ellos para el plano norte+este.
    let sdn = None;
    let qkc = None;

    let svc = DkmsService::new(
        cfg.clone(),
        metrics.clone(),
        pool,
        pending,
        buckets,
        sae_binding,
        sdn,
        qkc,
        peer_client,
    );

    // ─── TLS servidor ─────────────────────────────────────────────────
    let sae_tls = common::tls::server_config(
        &cfg.tls.cert_path,
        &cfg.tls.key_path,
        Some(cfg.tls.sae_client_ca.as_path()),
    )
    .context("sae tls")?;
    let peer_tls = common::tls::server_config(
        &cfg.tls.cert_path,
        &cfg.tls.key_path,
        Some(cfg.tls.peer_dkms_ca.as_path()),
    )
    .context("peer dkms tls")?;

    // ─── Listeners ────────────────────────────────────────────────────
    let http_task = tokio::spawn({
        let svc = svc.clone();
        let listen = cfg.listen.clone();
        async move { etsi_http::serve(svc, &listen, sae_tls, peer_tls).await }
    });
    let grpc_task = tokio::spawn({
        let svc = svc.clone();
        let addr = cfg.listen.grpc_addr;
        async move { grpc_server::serve(svc, addr).await }
    });
    let bg_task = tokio::spawn({
        let svc = svc.clone();
        async move { svc.run_background_tasks().await.map_err(anyhow::Error::from) }
    });

    let _ = cli; // (silencia warning si no se usa)
    let _: PathBuf = cfg.tls.cert_path.clone(); // (mantener type para futuras checks)

    tokio::select! {
        r = http_task => r??,
        r = grpc_task => r??,
        r = bg_task   => r??,
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
