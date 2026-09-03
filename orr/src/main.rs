//! ORR binary entry point.

#![forbid(unsafe_code)]
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
    // Claves fuera de swap y de core dumps (best-effort, ver common::hardening).
    common::hardening::harden_process();
    // Proveedor rustls: necesario si el anuncio al SDN va por mTLS (https).
    common::tls_pqc::ensure_process_default().map_err(anyhow::Error::msg)?;
    let _cli = Cli::parse();

    let cfg: OrrConfig = common::config::load_config("orr")?;
    info!(?cfg, "orr starting");
    // Self-check del intercambio de claves TLS (híbrido post-cuántico) con la
    // identidad del nodo, si la hay; sin identidad no hay TLS que comprobar.
    if let Some(t) = &cfg.tls {
        common::tls_pqc::self_check_hybrid_kx_files(&t.cert_path, &t.key_path)
            .map_err(anyhow::Error::msg)?;
    }
    // Identidad TLS del proceso: el servidor gRPC (mTLS por defecto) y los
    // canales hacia los pares. Sin identidad no se arranca en claro a
    // escondidas: apagarlo es una decisión, y se escribe.
    orr::grpc_tls::install(cfg.tls.clone(), cfg.grpc_tls);
    if cfg.grpc_tls && cfg.tls.is_none() {
        anyhow::bail!(
            "grpc_tls está activado (por defecto) y exige la sección [tls] \
             (cert_path, key_path, control_plane_ca): el certificado de nodo de \
             este ORR, firmado por la CA de red (docker/gen-certs.sh <orr_id> <ip>). \
             Para ir en claro —sólo si DKMS y ORR comparten máquina o red interna \
             de confianza— pon grpc_tls = false explícitamente."
        );
    }

    let metrics = Metrics::new("orr");
    metrics.serve(cfg.metrics_addr.clone()).await?;

    let service = OrrService::new(cfg.clone(), metrics).await?;

    // Anuncio periódico a la SDN. Fuera del `select!`: si la SDN no está o no
    // hay `sdn_http_url`, el ORR sigue enrutando igual.
    if let Some(announcer) = orr::sdn_announce::SdnAnnouncer::from_config(
        &cfg,
        service.peers.clone(),
        service.identity.clone(),
    ) {
        tokio::spawn(announcer.run());
    }

    // Línea de estado periódica. Es lo único que hay para ver desde fuera si
    // el bootstrap ha convergido con cada par; ver `orr::stats`.
    orr::stats::spawn_state_logger(
        service.stats.clone(),
        service.peers.clone(),
        std::time::Duration::from_secs(5),
    );

    let grpc = tokio::spawn({
        let svc = service.clone();
        async move { orr::grpc_server::serve(svc, &cfg.grpc_addr, cfg.grpc_tls).await }
    });

    tokio::select! {
        r = grpc => { r??; }
        _ = signal::ctrl_c() => info!("ctrl-c received, shutting down"),
    }
    Ok(())
}
