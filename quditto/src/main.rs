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

    /// TLS del ETSI-014: `on` (mTLS, default — por este canal viajan los pads
    /// OTP) u `off` (claro; SOLO si el QKC y este quditto comparten host).
    #[arg(long, default_value = "on", env = "QUDITTO_TLS")]
    tls: String,

    /// Cert de servidor (PEM, firmado por la CA de red). Obligatorio con
    /// `--tls on`.
    #[arg(long, env = "QUDITTO_TLS_CERT")]
    tls_cert: Option<std::path::PathBuf>,

    /// Clave privada del cert (PEM, ML-DSA seed-only de gen-certs.sh).
    #[arg(long, env = "QUDITTO_TLS_KEY")]
    tls_key: Option<std::path::PathBuf>,

    /// CA de red: verifica el cert cliente del QKC (obligatorio con TLS).
    #[arg(long, env = "QUDITTO_TLS_CLIENT_CA")]
    tls_client_ca: Option<std::path::PathBuf>,
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    // Logging mínimo: RUST_LOG si está, si no info.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // El provider PQC como default del proceso, como los otros cuatro
    // binarios: aunque hoy quditto no abra TLS propio, cualquier TLS que
    // gane la carrera después (o el [tls] opcional de su ETSI-014) debe
    // salir con el KX híbrido y ML-DSA, nunca con los defaults clásicos.
    common::tls_pqc::ensure_process_default().map_err(anyhow::Error::msg)?;

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
    // mTLS POR DEFECTO (2026-08-31): este servidor entrega los pads OTP en el
    // body — en claro, quien lea el canal lee la clave del enlace sin
    // criptoanálisis ninguno. Sin certs no se arranca (y se dice qué falta);
    // el claro es un opt-out explícito, como grpc_tls.
    let tls = match cli.tls.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => {
            let (Some(cert), Some(key), Some(ca)) =
                (&cli.tls_cert, &cli.tls_key, &cli.tls_client_ca)
            else {
                anyhow::bail!(
                    "quditto sirve material de clave: su ETSI-014 va con mTLS por defecto y \
                     faltan --tls-cert/--tls-key/--tls-client-ca (env QUDITTO_TLS_CERT/KEY/\
                     CLIENT_CA). Genera la identidad con docker/gen-certs.sh <id> <ip> ./certs \
                     o, SOLO si el QKC y este quditto comparten host o red interna de \
                     confianza, arranca con QUDITTO_TLS=off"
                );
            };
            // Con identidad ya hay algo que auto-comprobar: el mismo
            // self-check de arranque que los otros binarios.
            common::tls_pqc::self_check_hybrid_kx_files(cert, key).map_err(anyhow::Error::msg)?;
            Some(
                common::tls::server_config(cert, key, Some(ca))
                    .map_err(|e| anyhow::anyhow!("quditto [tls]: {e}"))?,
            )
        }
        "off" | "0" | "false" => {
            tracing::warn!(
                "quditto ETSI-014 EN CLARO (QUDITTO_TLS=off): los pads OTP viajan sin cifrar — \
                 solo mismo host o red interna de confianza"
            );
            None
        }
        other => anyhow::bail!("--tls '{other}' no es on|off"),
    };
    info!(?cfg, tls = tls.is_some(), "quditto starting");

    let svc = QudittoService::new(cfg);

    let minter = tokio::spawn({
        let svc = svc.clone();
        async move { svc.run_minter().await }
    });
    let http = tokio::spawn({
        let svc = svc.clone();
        let addr = cli.listen.clone();
        async move { quditto::server::serve(svc, &addr, tls).await }
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
