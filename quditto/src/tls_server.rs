//! Listener mTLS del ETSI-014 de quditto.
//!
//! Por este puerto salen los **pads OTP en claro dentro del body**: quien lea
//! este canal lee la clave del enlace QKD simulado, sin necesidad de ningún
//! ordenador cuántico. Por eso desde 2026-08-31 va con TLS (híbrido
//! post-cuántico + certs ML-DSA, el provider del proceso) y cert de cliente
//! OBLIGATORIO contra la CA de red — el QKC ya presenta su identidad de nodo
//! cuando `quditto_url` es `https://`. El claro queda como opt-out explícito
//! (`QUDITTO_TLS=off`) para el sidecar co-localizado.
//!
//! Copia deliberada del patrón `sdn/src/mtls.rs` / `qkc/src/mtls_admin.rs`
//! sin extracción de identidad (ETSI-014 no ata ids del body al cert); la
//! consolidación en `common` es un follow-on (docs/SECURITY.md §Fase 3).

use std::sync::Arc;

use anyhow::Result;
use axum::{body::Body, Router};
use hyper::{body::Incoming, service::service_fn, Request};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{debug, error, info, warn};

/// Sirve `router` sobre TLS con cert de cliente obligatorio. Bucle
/// resiliente: un fallo de accept/handshake/conexión nunca tumba el listener.
pub async fn serve_mtls(
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    router: Router,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let addr = listener.local_addr().ok();
    info!(?addr, "quditto ETSI-014 https (mTLS) listening");

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "quditto tcp accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();

        tokio::spawn(async move {
            let tls_stream = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                acceptor.accept(tcp),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    // Un QKC con quditto_url http:// (o sin cert) acaba aquí:
                    // el desajuste es ruidoso en los dos lados.
                    warn!(%peer_addr, error = %e, "quditto tls handshake failed");
                    return;
                }
                Err(_) => {
                    warn!(%peer_addr, "tls handshake timeout (B7)");
                    return;
                }
            };
            let svc = service_fn(move |req: Request<Incoming>| {
                let mut router = router.clone();
                async move {
                    let req: Request<Body> = req.map(Body::new);
                    router.call(req).await
                }
            });
            let io = TokioIo::new(tls_stream);
            let mut builder = auto::Builder::new(TokioExecutor::new());
            builder.http1().timer(TokioTimer::new());
            builder
                .http2()
                .timer(TokioTimer::new())
                .keep_alive_interval(std::time::Duration::from_secs(20));
            if let Err(e) = builder.serve_connection(io, svc).await {
                debug!(%peer_addr, error = %e, "quditto conn closed");
            }
        });
    }
}
