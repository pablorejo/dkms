//! quditto binary entry point.
//!
//! Lanza:
//!   1. El generador de claves (background, tasa `R(d) = R₀·10^(-α·d/10)`).
//!   2. El servidor HTTP ETSI 014.
//!
//! Termina con Ctrl-C.

use anyhow::Result;
use clap::Parser;
use quditto::{config::QudittoConfig, service::QudittoService};
use tokio::signal;
use tracing::info;
use tracing_subscriber::EnvFilter;

/// Simulador de enlace QKD con buffer de claves aleatorias de
/// 256 bits, servido por HTTP ETSI 014.
#[derive(Parser, Debug)]
#[command(name = "quditto", version, about, long_about = None)]
struct Cli {
    /// Dirección de escucha HTTP, `host:port`.
    #[arg(long, default_value = "0.0.0.0:8080", env = "QUDITTO_LISTEN")]
    listen: String,

    /// `R₀` — tasa base de generación cuando la distancia es 0
    /// (claves/s).
    #[arg(long, default_value_t = 1000.0, env = "QUDITTO_R0")]
    r0: f64,

    /// `α` — atenuación de fibra en dB/km.
    #[arg(long, default_value_t = 0.2, env = "QUDITTO_ALPHA")]
    alpha: f64,

    /// Distancia del enlace simulado en km.
    #[arg(long, default_value_t = 0.0, env = "QUDITTO_DISTANCE")]
    distance: f64,

    /// Capacidad máxima del buffer en número de claves.
    /// Si está lleno los ticks de generación se descartan.
    #[arg(long, default_value_t = 8192, env = "QUDITTO_MAX_BUFFER")]
    max_buffer: u64,

    /// Tamaño de la clave en bits. Por la spec del proyecto: 256.
    /// Otros valores fallan en validación.
    #[arg(long, default_value_t = 256, env = "QUDITTO_KEY_SIZE_BITS")]
    key_size_bits: u32,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Logging mínimo: RUST_LOG si está, si no info.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let cfg = QudittoConfig {
        listen: cli.listen.clone(),
        r0: cli.r0,
        alpha: cli.alpha,
        distance_km: cli.distance,
        max_buffer_keys: cli.max_buffer,
        key_size_bits: cli.key_size_bits,
    };
    if let Err(e) = cfg.validate() {
        eprintln!("invalid config: {e}");
        std::process::exit(2);
    }
    info!(?cfg, "quditto starting");

    let svc = QudittoService::new(cfg);

    let minter = tokio::spawn({
        let svc = svc.clone();
        async move { svc.run_minter().await }
    });
    let http = tokio::spawn({
        let svc = svc.clone();
        let addr = cli.listen.clone();
        async move { quditto::server::serve(svc, &addr).await }
    });

    tokio::select! {
        r = minter => {
            if let Err(e) = r? { return Err(e.into()); }
        }
        r = http => {
            r??;
        }
        _ = signal::ctrl_c() => {
            info!("ctrl-c received, shutting down");
        }
    }

    Ok(())
}
