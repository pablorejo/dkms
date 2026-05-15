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
use tracing::{info, warn};

use common::{logging, metrics::Metrics};
use dkms::{
    config::DkmsConfig,
    etsi_http, grpc_server,
    peer_client::PeerHttpClient,
    sae_binding::{SaeBindingCache, SaeResolver, SdnSaeResolver, StaticSaeResolver},
    service::DkmsService,
    southbound::{OrrClient, QkcClient, SdnClient},
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

    // ─── Cliente DKMS↔DKMS (HTTP/2 ETSI 020) ──────────────────────────
    // Construido sólo si algún peer usa `transport = "http"`. En modo
    // ORR-only el PeerHttpClient no se invoca, así que evitamos pagar
    // el coste (y los problemas de TLS backend) cuando nadie lo necesita.
    use dkms::config::PeerTransport;
    let needs_http_peer_client = cfg
        .peers
        .values()
        .any(|p| p.transport == PeerTransport::Http);
    let peer_client = if needs_http_peer_client {
        Some(Arc::new(
            PeerHttpClient::build(
                &cfg.tls.cert_path,
                &cfg.tls.key_path,
                &cfg.tls.peer_dkms_ca,
                cfg.request.clone(),
            )
            .context("peer http client")?,
        ))
    } else {
        info!("no http transport peers configured; skipping peer http client");
        None
    };

    // ─── Clientes sur (SDN / QKC / ORR) ──────────────────────────────
    // Cada uno intenta conectar; si falla, se loguea y se sigue con
    // `None`. El DKMS arranca aunque sus vecinos no estén listos —
    // útil en bring-up donde los pods se inician en cualquier orden.
    let sdn = match SdnClient::connect(&cfg.southbound, None).await {
        Ok(c) => {
            info!(endpoint = %cfg.southbound.sdn_endpoint, "sdn client connected");
            Some(std::sync::Arc::new(c))
        }
        Err(e) => {
            warn!(error = %e, endpoint = %cfg.southbound.sdn_endpoint, "sdn unreachable at boot; continuing without it");
            None
        }
    };

    // ─── SAE binding resolver ─────────────────────────────────────────
    // Si la SDN está disponible, usamos un `SdnSaeResolver` que consulta
    // `GetSaeBinding` por gRPC. La `SaeBindingCache` envuelve cualquier
    // resolver y cachea con TTL — "primera vez SDN, siguientes en
    // memoria" sale gratis. El mapa estático `[sae_bindings]` del config
    // sólo se usa cuando la SDN no está cableada (local-dev/CI).
    let resolver: Arc<dyn SaeResolver> = if let Some(sdn_client) = &sdn {
        info!("using SdnSaeResolver (SDN-backed)");
        Arc::new(SdnSaeResolver::new(sdn_client.clone()))
    } else {
        let static_resolver = Arc::new(StaticSaeResolver::new());
        for (sae, node) in &cfg.sae_bindings {
            static_resolver.insert(
                common::ids::SaeId::new(sae.clone()),
                common::ids::NodeId::new(node.clone()),
            );
        }
        if !cfg.sae_bindings.is_empty() {
            info!(
                n = cfg.sae_bindings.len(),
                "loaded static sae bindings from config (SDN unreachable)"
            );
        }
        static_resolver
    };
    let sae_binding = Arc::new(SaeBindingCache::new(
        resolver,
        cfg.sae_binding.ttl_secs,
        cfg.sae_binding.max_entries,
    ));

    // Suscripción a `StreamTopology`: cuando la SDN avisa de un
    // cambio, invalidamos la cache de SAE bindings. La siguiente
    // request volverá a preguntar a la SDN y la cache se rellenará.
    // Reconnect con backoff exponencial si el stream cae.
    if let Some(sdn_client) = sdn.clone() {
        let cache = sae_binding.clone();
        tokio::spawn(async move {
            let mut backoff_ms: u64 = 250;
            loop {
                match sdn_client.stream_topology().await {
                    Ok(mut stream) => {
                        info!("dkms.topology_subscriber connected");
                        backoff_ms = 250;
                        while let Some(item) = stream.message().await.transpose() {
                            match item {
                                Ok(ev) => {
                                    cache.invalidate_all().await;
                                    info!(
                                        version = ev.version,
                                        "dkms.sae_binding_cache invalidated"
                                    );
                                }
                                Err(s) => {
                                    warn!(status = %s, "dkms.topology stream broken");
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => warn!(error = %e, "dkms.topology subscribe failed; retrying"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(5_000);
            }
        });
    }
    let qkc = match QkcClient::connect(&cfg.southbound, None).await {
        Ok(c) => {
            info!(endpoint = %cfg.southbound.qkc_endpoint, "qkc client connected");
            Some(std::sync::Arc::new(c))
        }
        Err(e) => {
            warn!(error = %e, endpoint = %cfg.southbound.qkc_endpoint, "qkc unreachable at boot; continuing without it");
            None
        }
    };
    // ORR es opcional por config: si `southbound.orr_endpoint` está
    // vacío/ausente, `connect_opt` devuelve `Ok(None)` sin loguear.
    let orr = match OrrClient::connect_opt(&cfg.southbound, None).await {
        Ok(Some(c)) => {
            info!(
                endpoint = %cfg.southbound.orr_endpoint.as_deref().unwrap_or(""),
                "orr client connected",
            );
            Some(std::sync::Arc::new(c))
        }
        Ok(None) => None,
        Err(e) => {
            warn!(error = %e, "orr unreachable at boot; continuing without it");
            None
        }
    };

    let svc = DkmsService::new(
        cfg.clone(),
        metrics.clone(),
        pool,
        pending,
        buckets,
        sae_binding,
        sdn,
        qkc,
        orr,
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
