//! Plano de gestión del DKMS — gRPC `DkmsControl` para el orquestador.
//!
//! La capa SAE (ETSI 014) y la capa DKMS↔DKMS (ETSI 020) viven en
//! [`crate::etsi_http`]. Esto es **solo** la superficie de mgmt:
//! registro de SAEs, drenaje, health, snapshots de buffer.

use common::proto::{
    common::v1::Status as ProtoStatus,
    dkms::v1::{
        dkms_control_server::{DkmsControl, DkmsControlServer},
        health_response, BufferState, BufferStateRequest, DeregisterSaeRequest, DrainRequest,
        HealthRequest, HealthResponse, ListSaesRequest, RegisterSaeRequest, RegisterSaeResponse,
        SaeInfo,
    },
};
use std::time::Duration;

use tonic::{transport::Server, Request, Response, Status};
use tracing::{field, info, instrument, warn};

use common::ids::SaeId;

use crate::service::DkmsService;

pub struct DkmsGrpc {
    pub svc: DkmsService,
}

#[tonic::async_trait]
impl DkmsControl for DkmsGrpc {
    type ListSaesStream =
        tokio_stream::wrappers::ReceiverStream<std::result::Result<SaeInfo, Status>>;

    #[instrument(skip_all)]
    async fn register_sae(
        &self,
        req: Request<RegisterSaeRequest>,
    ) -> std::result::Result<Response<RegisterSaeResponse>, Status> {
        let m = req.into_inner();
        let sae_id = m.sae_id.map(|s| SaeId::new(s.value)).unwrap_or_else(|| SaeId::new(""));
        if sae_id.as_str().is_empty() {
            return Err(Status::invalid_argument("sae_id required"));
        }
        self.svc.buckets.set_limits(
            &sae_id,
            m.rate_keys_per_sec.max(1),
            m.burst_keys.max(m.rate_keys_per_sec).max(1),
        );
        Ok(Response::new(RegisterSaeResponse {
            registration_id: uuid::Uuid::new_v4().to_string(),
            expires_at_unix_ms: chrono::Utc::now().timestamp_millis() + 24 * 3600 * 1000,
        }))
    }

    #[instrument(skip_all)]
    async fn deregister_sae(
        &self,
        req: Request<DeregisterSaeRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        let _sae = req
            .into_inner()
            .sae_id
            .map(|s| SaeId::new(s.value))
            .unwrap_or_else(|| SaeId::new(""));
        // Token buckets quedan en memoria por simplicidad — al SAE no se le
        // entregará nada hasta que `register_sae` lo vuelva a admitir
        // porque mTLS lo bloqueará en el plano norte si el cert se ha
        // revocado.
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn list_saes(
        &self,
        _req: Request<ListSaesRequest>,
    ) -> std::result::Result<Response<Self::ListSaesStream>, Status> {
        let (_tx, rx) =
            tokio::sync::mpsc::channel::<std::result::Result<SaeInfo, Status>>(16);
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    #[instrument(skip_all, fields(grace_secs = field::Empty))]
    async fn drain(
        &self,
        req: Request<DrainRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        let grace_secs = req.into_inner().grace_seconds;
        let grace = if grace_secs == 0 {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(grace_secs as u64)
        };
        tracing::Span::current().record("grace_secs", grace.as_secs());

        // 1) Cortar la admisión: el AdmissionLayer empezará a devolver 503.
        self.svc.admission.close();
        let initial_inflight = self.svc.admission.inflight();
        info!(initial_inflight, "drain: stopped accepting new requests");

        // 2) Esperar a que las requests en curso terminen, con tope `grace`.
        match tokio::time::timeout(grace, self.svc.admission.wait_drained()).await {
            Ok(_) => info!("drain: in-flight requests finished cleanly"),
            Err(_) => warn!(
                remaining = self.svc.admission.inflight(),
                "drain: grace timeout exhausted, proceeding with state wipe"
            ),
        }

        // 3) Wipe del estado sensible — los `Zeroizing<Vec<u8>>` se ocupan
        //    de poner los bytes a cero al hacer drop.
        self.svc.pending.clear();
        self.svc.pool.clear_all();
        info!("drain: pending store and transport buffers cleared");

        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn health(
        &self,
        _req: Request<HealthRequest>,
    ) -> std::result::Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse {
            status: health_response::Status::Healthy as i32,
        }))
    }

    #[instrument(skip_all)]
    async fn get_buffer_state(
        &self,
        req: Request<BufferStateRequest>,
    ) -> std::result::Result<Response<BufferState>, Status> {
        let m = req.into_inner();
        let _local = m.local_sae.map(|s| s.value).unwrap_or_default();
        let remote = m.remote_sae.map(|s| s.value).unwrap_or_default();
        // El DKMS Rust agrega buffers por peer-DKMS, no por par de SAEs.
        // Devolvemos el snapshot del DKMS dueño del SAE remoto, si lo
        // tenemos. Si no hay binding cacheado, devolvemos ceros (el
        // orquestador puede pedirle a la SDN el binding).
        let peer_node = self
            .svc
            .sae_binding
            .resolve(&SaeId::new(remote))
            .await
            .ok();
        let snapshot = peer_node
            .map(|n| self.svc.pool.for_peer(n.as_str()))
            .map(|pb| (pb.enc.len(), pb.dec.len()))
            .unwrap_or((0, 0));
        Ok(Response::new(BufferState {
            keys_available: snapshot.0 as u64,
            keys_reserved: snapshot.1 as u64,
            bytes_buffered: 0,
            last_replenished_unix_ms: chrono::Utc::now().timestamp_millis(),
        }))
    }
}

pub async fn serve(svc: DkmsService, addr: std::net::SocketAddr) -> anyhow::Result<()> {
    info!(%addr, "dkms gRPC listening");
    Server::builder()
        .add_service(DkmsControlServer::new(DkmsGrpc { svc }))
        .serve(addr)
        .await?;
    Ok(())
}
