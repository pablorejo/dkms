//! Prometheus metrics registry + tiny HTTP exporter.
//!
//! Each module gets its own `Registry` (so process restarts produce clean
//! state) but the construction pattern is identical:
//!
//! ```ignore
//! let metrics = common::metrics::Metrics::new("qkc");
//! metrics.serve("0.0.0.0:9100").await?;
//! ```

use std::{net::SocketAddr, sync::Arc};

use prometheus::{Encoder, Registry, TextEncoder};
use thiserror::Error;
use tokio::net::TcpListener;
use tracing::{error, info};

#[derive(Debug, Error)]
pub enum MetricsError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("prometheus: {0}")]
    Prometheus(#[from] prometheus::Error),
    #[error("addr parse: {0}")]
    AddrParse(#[from] std::net::AddrParseError),
}

#[derive(Clone)]
pub struct Metrics {
    pub registry: Arc<Registry>,
    pub service: String,
}

impl Metrics {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            registry: Arc::new(Registry::new()),
            service: service.into(),
        }
    }

    /// Start a minimal HTTP server exposing `/metrics`.
    ///
    /// Spawned task lives until the process exits. Each request encodes
    /// the registry into the Prometheus text exposition format.
    pub async fn serve(&self, addr: impl AsRef<str>) -> Result<(), MetricsError> {
        let addr: SocketAddr = addr.as_ref().parse()?;
        let listener = TcpListener::bind(addr).await?;
        let registry = self.registry.clone();
        let service = self.service.clone();

        info!(addr = %addr, service = %service, "metrics endpoint listening");

        tokio::spawn(async move {
            loop {
                let (mut stream, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(e) => {
                        error!(error = %e, "metrics: accept failed");
                        continue;
                    }
                };
                let registry = registry.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let mfs = registry.gather();
                    let encoder = TextEncoder::new();
                    let mut body = Vec::with_capacity(2048);
                    let _ = encoder.encode(&mfs, &mut body);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n",
                        encoder.format_type(),
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        Ok(())
    }
}
