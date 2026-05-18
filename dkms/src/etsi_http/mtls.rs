//! Listener TLS con verificación de cert cliente + inyección de
//! [`PeerIdentity`] en cada request.
//!
//! Patrón:
//!
//! 1. `TcpListener::accept()` por cada conexión.
//! 2. `tokio_rustls::TlsAcceptor::accept()` para terminar TLS.
//! 3. Extraer cert(s) verificados de la sesión y construir un
//!    [`PeerIdentity`].
//! 4. Servir la conexión con `hyper_util::server::conn::auto` y un
//!    `service_fn` que, por cada request, copia el `PeerIdentity` a las
//!    extensiones de la request antes de delegar al `axum::Router`.
//!
//! Se sirve sobre **HTTP/2** forzado: ambos planos del DKMS hablan HTTP/2.
//! Sin upgrade/ALPN dance — los SAEs y DKMS clientes ya saben que tienen
//! que abrir HTTP/2.

use std::{net::SocketAddr, sync::Arc};

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

use super::auth::PeerIdentity;

/// Sirve `router` sobre TLS con verificación de cert cliente.
///
/// El bucle es resiliente: errores de `accept`, handshake o conexión
/// nunca rompen el listener.
pub async fn serve_mtls(
    addr: SocketAddr,
    tls_config: Arc<rustls::ServerConfig>,
    router: Router,
    plane_label: &'static str,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, plane = plane_label, "dkms https listening");

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "tcp accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    warn!(%peer_addr, error = %e, "tls handshake failed");
                    return;
                }
            };

            let peer_id = {
                let (_io, session) = tls_stream.get_ref();
                PeerIdentity::from_verified(session.peer_certificates())
            };

            let svc = service_fn(move |req: Request<Incoming>| {
                let mut router = router.clone();
                let pid = peer_id.clone();
                async move {
                    let mut req: Request<Body> = req.map(Body::new);
                    req.extensions_mut().insert(pid);
                    router.call(req).await
                }
            });

            let io = TokioIo::new(tls_stream);
            let mut builder = auto::Builder::new(TokioExecutor::new());
            // Auto-detect HTTP/1.1 vs HTTP/2 from the stream prefix
            // (or ALPN when present). Both branches need a timer:
            //
            // * The DKMS↔DKMS client (reqwest) negotiates HTTP/2
            //   prior-knowledge — `http2().keep_alive_interval(...)`
            //   requires `TokioTimer` or hyper 1.x panics.
            // * The SAE plane is also hit by Python `requests` (used
            //   by the loadtest pod when it talks directly to the
            //   Service intra-cluster, bypassing the nginx-ingress
            //   that does HTTP/1↔H2 translation). That client speaks
            //   HTTP/1.1 over the same TLS port, so we keep h1 enabled
            //   too. Without this the loadtest got status_code=0
            //   connection errors (test_sae.runtime_dkms_base_url with
            //   LOADTEST_DKMS_ENDPOINTS).
            builder
                .http1()
                .timer(TokioTimer::new());
            builder
                .http2()
                .timer(TokioTimer::new())
                .keep_alive_interval(std::time::Duration::from_secs(20));

            if let Err(e) = builder.serve_connection(io, svc).await {
                debug!(%peer_addr, error = %e, "conn closed");
            }
        });
    }
}
