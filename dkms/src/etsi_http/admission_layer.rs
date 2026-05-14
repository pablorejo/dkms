//! `tower::Layer` que aplica el control de admisión a un `axum::Router`.
//!
//! Por cada request:
//!
//! * Si `Admission::is_accepting() == false`, contesta **503** sin tocar el
//!   handler.
//! * Si está aceptando, toma un [`crate::admission::InflightGuard`] y lo
//!   suelta cuando termina el handler (éxito o panic), decrementando
//!   `inflight` y, si llega a 0, notificando a quien esté esperando en
//!   `wait_drained()` (el drenaje del `Drain` RPC).

use std::{
    convert::Infallible,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use tower::{Layer, Service};

use crate::admission::Admission;

#[derive(Clone)]
pub struct AdmissionLayer {
    admission: Arc<Admission>,
}

impl AdmissionLayer {
    pub fn new(admission: Arc<Admission>) -> Self {
        Self { admission }
    }
}

impl<S> Layer<S> for AdmissionLayer {
    type Service = AdmissionService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        AdmissionService {
            inner,
            admission: self.admission.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AdmissionService<S> {
    inner: S,
    admission: Arc<Admission>,
}

impl<S> Service<Request<Body>> for AdmissionService<S>
where
    S: Service<Request<Body>, Response = Response, Error = Infallible> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        if !self.admission.is_accepting() {
            return Box::pin(async {
                Ok((
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({"message": "dkms is draining, not accepting new requests"})),
                )
                    .into_response())
            });
        }

        let guard = self.admission.acquire();

        // Patrón estándar tower: swap del inner con un clon para que el
        // futuro use el inner sobre el que ya hicimos poll_ready, y el
        // próximo poll_ready opere sobre el clon fresco.
        let cloned = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, cloned);

        Box::pin(async move {
            let resp = inner.call(req).await;
            drop(guard);
            resp
        })
    }
}
