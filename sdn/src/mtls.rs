//! Listener mTLS del SDN + identidad del peer por certificado.
//!
//! El plano de control del SDN (registro de nodos, rebind de SAEs) cruza
//! instituciones y en despliegue multi-host va sobre TLS. Este módulo sirve
//! un `axum::Router` sobre TLS con verificación de cert cliente (trust root
//! = CA de red) e inyecta un [`PeerCertIdentity`] en las extensiones de cada
//! request, para que los handlers puedan atar el id del body al SAN del cert
//! (ver `http_api::require_identity_match`).
//!
//! Es una copia deliberada del patrón de `dkms/src/etsi_http/mtls.rs`: la
//! consolidación en `common` es un follow-on (docs/SECURITY.md §Fase 3), no
//! se hace ahora para no arrastrar axum/x509 a `common` ni acoplar sdn↔dkms.

static ACCEPT_FAILED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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

/// Identidad del peer extraída del cert cliente verificado por rustls.
/// `None` en `san` si el cert no trae un SAN utilizable. Se inyecta a nivel
/// de conexión; los handlers la leen vía `Option<Extension<PeerCertIdentity>>`.
#[derive(Clone, Debug)]
pub struct PeerCertIdentity {
    /// Primer SAN URI/DNS o CN del cert, sin normalizar el esquema.
    pub san: Option<String>,
}

impl PeerCertIdentity {
    fn from_verified(certs: Option<&[rustls::pki_types::CertificateDer<'_>]>) -> Self {
        let san = certs
            .and_then(|c| c.first())
            .and_then(|leaf| extract_san_identifier(leaf.as_ref()));
        Self { san }
    }
}

use common::cert_identity::extract_san_identifier;

/// Sirve `router` sobre TLS con verificación de cert cliente, inyectando
/// [`PeerCertIdentity`] en cada request. Bucle resiliente: un fallo de
/// accept/handshake/conn nunca tumba el listener.
pub async fn serve_mtls(
    listener: TcpListener,
    tls_config: Arc<rustls::ServerConfig>,
    router: Router,
) -> Result<()> {
    let acceptor = TlsAcceptor::from(tls_config);
    let addr = listener.local_addr().ok();
    info!(?addr, "sdn https (mTLS) listening");

    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                let n = ACCEPT_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n) {
                    error!(failed = n + 1, error = %e, "tcp accept failed");
                }
                // EMFILE y compañía: sin pausa esto es un bucle caliente (R8).
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
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
                    warn!(%peer_addr, error = %e, "tls handshake failed");
                    return;
                }
                Err(_) => {
                    warn!(%peer_addr, "tls handshake timeout (B7)");
                    return;
                }
            };
            let (peer_id, kx) = {
                let (_io, session) = tls_stream.get_ref();
                (
                    PeerCertIdentity::from_verified(session.peer_certificates()),
                    session.negotiated_key_exchange_group().map(|g| g.name()),
                )
            };
            tracing::debug!(%peer_addr, kx = ?kx, "tls.conn accepted");
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
            builder.http1().timer(TokioTimer::new());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_san_yields_none() {
        assert!(extract_san_identifier(b"not a cert").is_none());
    }
}
