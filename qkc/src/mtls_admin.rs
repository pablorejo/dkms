//! Listener mTLS del HTTP admin del QKC.
//!
//! Por este puerto entra `POST /forwarding-table`: quien lo controla decide
//! por dónde viaja cada frame OTP de la red. En multi-host va sobre TLS con
//! verificación de cert cliente (trust root = CA de red): la autorización es
//! «presenta un cert de la net-ca», el mismo listón que el gRPC del ORR. No
//! hace falta extraer la identidad del SAN — no hay un `from` en el body que
//! atar, a diferencia del registro del SDN.
//!
//! Es una copia deliberada de `sdn/src/mtls.rs::serve_mtls` sin la extensión
//! de identidad: la consolidación en `common` es un follow-on
//! (docs/SECURITY.md §Fase 3), no se hace ahora para no arrastrar
//! axum/hyper-util a `common` ni acoplar qkc↔sdn.

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

/// Sirve `router` sobre TLS con cert de cliente OBLIGATORIO. Bucle
/// resiliente: un fallo de accept/handshake/conexión nunca tumba el listener.
pub async fn serve_mtls(
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    router: Router,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let addr = listener.local_addr().ok();
    info!(?addr, "qkc.http_admin https (mTLS) listening");

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "qkc.http_admin tcp accept failed");
                continue;
            }
        };
        let acceptor = acceptor.clone();
        let router = router.clone();

        tokio::spawn(async move {
            let tls_stream = match acceptor.accept(tcp).await {
                Ok(s) => s,
                Err(e) => {
                    // Un push en claro de una SDN sin [tls] acaba aquí: el
                    // desajuste de despliegue es ruidoso en los dos lados.
                    warn!(%peer_addr, error = %e, "qkc.http_admin tls handshake failed");
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
                debug!(%peer_addr, error = %e, "qkc.http_admin conn closed");
            }
        });
    }
}
