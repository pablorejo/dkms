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

use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::Result;
use axum::{body::Body, Router};
use hyper::{body::Incoming, service::service_fn, Request};
use hyper_util::{
    rt::{TokioExecutor, TokioIo, TokioTimer},
    server::conn::auto,
};
use tokio_rustls::TlsAcceptor;
use tower::Service;
use tracing::{debug, error, info, warn};

use super::auth::PeerIdentity;

/// Contadores del handshake TLS de un plano, volcados cada
/// [`STATS_PERIOD`] en una línea `tls.stats`.
///
/// Existe para poder comparar el coste de los certificados: con conexiones
/// keep-alive el handshake se amortiza y **no** se ve en la latencia por
/// petición del cliente de carga, así que sin esto el sobrecoste de una
/// firma ML-DSA (3309 B de firma, cadena de ~4 KB) frente a RSA sería
/// invisible en los datos de campaña. Los fallos van aparte porque
/// "handshakes lentos" y "handshakes que no cierran" son bugs distintos.
#[derive(Default)]
struct TlsStats {
    accepted: AtomicU64,
    failed: AtomicU64,
    total_us: AtomicU64,
    max_us: AtomicU64,
}

/// 30 s: con la observación de campaña (600 s) da ~20 muestras, suficientes
/// para ver la evolución y no solo un promedio. Con 60 s una corrida corta
/// terminaba antes del primer volcado útil (el primer tick de `interval` es
/// inmediato y sale vacío) y no dejaba ni una línea.
const STATS_PERIOD: Duration = Duration::from_secs(30);

impl TlsStats {
    fn record_ok(&self, us: u64) {
        self.accepted.fetch_add(1, Ordering::Relaxed);
        self.total_us.fetch_add(us, Ordering::Relaxed);
        self.max_us.fetch_max(us, Ordering::Relaxed);
    }

    /// Vuelca y pone a cero. Devuelve `None` si no hubo actividad, para no
    /// llenar el log de líneas vacías en los planos ociosos.
    fn drain(&self) -> Option<(u64, u64, f64, f64)> {
        let n = self.accepted.swap(0, Ordering::Relaxed);
        let failed = self.failed.swap(0, Ordering::Relaxed);
        let total = self.total_us.swap(0, Ordering::Relaxed);
        let max = self.max_us.swap(0, Ordering::Relaxed);
        if n == 0 && failed == 0 {
            return None;
        }
        let avg_ms = if n > 0 {
            total as f64 / n as f64 / 1000.0
        } else {
            0.0
        };
        Some((n, failed, avg_ms, max as f64 / 1000.0))
    }
}

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
    let listener = common::net::bind_reuse_addr(addr).await?;
    info!(%addr, plane = plane_label, "dkms https listening");

    // Volcado periódico del coste del handshake (ver `TlsStats`).
    let stats = Arc::new(TlsStats::default());
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(STATS_PERIOD);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Some((n, failed, avg_ms, max_ms)) = stats.drain() {
                    info!(
                        plane = plane_label,
                        handshakes = n,
                        failed,
                        avg_ms = format_args!("{avg_ms:.1}"),
                        max_ms = format_args!("{max_ms:.1}"),
                        "tls.stats"
                    );
                }
            }
        });
    }

    // Cota de conexiones concurrentes (B7): junto al timeout de handshake evita
    // que N conexiones a medio handshake fijen fd+task indefinidamente.
    let conns = Arc::new(tokio::sync::Semaphore::new(4096));
    loop {
        let (tcp, peer_addr) = match listener.accept().await {
            Ok(p) => p,
            Err(e) => {
                error!(error = %e, "tcp accept failed");
                continue;
            }
        };
        let Ok(permit) = conns.clone().try_acquire_owned() else {
            debug!(%peer_addr, "tls: tope de conexiones, rechazo la nueva (B7)");
            continue;
        };
        let acceptor = acceptor.clone();
        let router = router.clone();
        let stats = stats.clone();

        tokio::spawn(async move {
            let _permit = permit;
            // Cronometrar el handshake: es la métrica clave para comparar el
            // coste de certs clásicos vs ML-DSA (campañas de certs). El
            // fallo se loguea con el detalle de rustls (p.ej. BadSignature,
            // UnknownIssuer) — es lo que distingue "cert de otra CA" de
            // "el cliente no mandó cert" en un despliegue real.
            let t0 = std::time::Instant::now();
            let tls_stream = match tokio::time::timeout(
                std::time::Duration::from_secs(10),
                acceptor.accept(tcp),
            )
            .await
            {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        %peer_addr,
                        plane = plane_label,
                        error = %e,
                        handshake_ms = t0.elapsed().as_millis() as u64,
                        "tls handshake failed"
                    );
                    return;
                }
                Err(_) => {
                    stats.failed.fetch_add(1, Ordering::Relaxed);
                    warn!(%peer_addr, plane = plane_label, "tls handshake timeout (B7)");
                    return;
                }
            };
            let handshake_us = t0.elapsed().as_micros() as u64;
            stats.record_ok(handshake_us);
            let handshake_ms = handshake_us / 1000;

            let (peer_id, kx) = {
                let (_io, session) = tls_stream.get_ref();
                (
                    PeerIdentity::from_verified(session.peer_certificates()),
                    session.negotiated_key_exchange_group().map(|g| g.name()),
                )
            };
            // Una línea por conexión aceptada, con la identidad que rustls
            // verificó y cuánto costó el handshake. En campañas: grep
            // "tls.conn" y compara handshake_ms entre arms RSA/ML-DSA.
            debug!(
                %peer_addr,
                plane = plane_label,
                san = peer_id.san_identifier.as_deref().unwrap_or("<none>"),
                handshake_ms,
                kx = ?kx,
                "tls.conn accepted"
            );

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
