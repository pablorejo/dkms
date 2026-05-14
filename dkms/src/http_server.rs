//! ETSI HTTP listener (axum).
//!
//! Para esta fase del rewrite los handlers ETSI 014/020 viven aún en el
//! crate `etsi/` como modelos. Cuando rehagamos DKMS engancharemos
//! aquí los routers que los consumen. Por ahora el server solo expone
//! `/healthz` y `/readyz` para que el binario arranque.

use axum::{response::IntoResponse, routing::get, Json, Router};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::info;

use crate::service::DkmsService;

#[derive(Serialize)]
struct Healthz {
    status: &'static str,
}

async fn healthz() -> impl IntoResponse {
    Json(Healthz { status: "ok" })
}

pub async fn serve(svc: DkmsService, addr: &str) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(healthz))
        .with_state(svc.clone());

    // TODO: when DKMS gets rewritten, mount ETSI 014/020 routers here
    // using the models in the `etsi` crate, and wrap the listener in
    // tokio_rustls when `svc.cfg.tls` is set.
    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "dkms HTTP listening (ETSI handlers pending rewrite)");
    axum::serve(listener, app).await?;
    Ok(())
}
