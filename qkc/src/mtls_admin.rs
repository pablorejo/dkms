//! Listener mTLS del HTTP admin del QKC.
//!
//! Por este puerto entra `POST /forwarding-table`: quien lo controla decide
//! por dónde viaja cada frame OTP de la red. En multi-host va sobre TLS con
//! verificación de cert cliente (trust root = CA de red), y además se extrae
//! la identidad del SAN ([`PeerCertIdentity`]) y se inyecta en las extensiones
//! de cada request: el push de la tabla sólo se acepta de la SDN (B1), no de
//! cualquier miembro de la red — un QKC/ORR/DKMS comprometido no debe poder
//! redirigir el tráfico de este nodo. La comprobación vive en el handler
//! (`http_admin::sdn_may_push`); aquí sólo se transporta la identidad.
//!
//! Es una copia deliberada de `sdn/src/mtls.rs::serve_mtls`: la consolidación
//! en `common` es un follow-on (docs/SECURITY.md §Fase 3), no se hace ahora
//! para no arrastrar axum/hyper-util a `common` ni acoplar qkc↔sdn.

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

/// Identidad del cliente mTLS, extraída del cert verificado por rustls e
/// inyectada a nivel de conexión en las extensiones de cada request. Los
/// handlers la leen con `Option<Extension<PeerCertIdentity>>`: la extensión
/// ausente significa listener en claro (sin `[tls]`), no «cliente anónimo».
#[derive(Clone, Debug)]
pub struct PeerCertIdentity {
    /// `node_id` del SAN `URI:dkms://<id>` de la hoja, en minúsculas.
    /// `None` si el cert (ya verificado contra la net-ca) no lleva ese SAN.
    pub node_id: Option<String>,
}

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
                let n = ACCEPT_FAILED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n) {
                    error!(failed = n + 1, error = %e, "qkc.http_admin tcp accept failed");
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
                    // Un push en claro de una SDN sin [tls] acaba aquí: el
                    // desajuste de despliegue es ruidoso en los dos lados.
                    warn!(%peer_addr, error = %e, "qkc.http_admin tls handshake failed");
                    return;
                }
                Err(_) => {
                    warn!(%peer_addr, "tls handshake timeout (B7)");
                    return;
                }
            };
            // Identidad del cliente, una vez por conexión (rustls ya verificó
            // la cadena contra la net-ca; aquí sólo se lee el SAN de la hoja).
            let peer_id = {
                let (_io, session) = tls_stream.get_ref();
                PeerCertIdentity {
                    node_id: session
                        .peer_certificates()
                        .and_then(common::cert_identity::node_id_from_certs),
                }
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
