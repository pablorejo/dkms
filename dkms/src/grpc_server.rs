//! Orchestrator-facing gRPC: registration, drain, health.

use common::proto::{
    common::v1::Status as ProtoStatus,
    dkms::v1::{
        dkms_control_server::{DkmsControl, DkmsControlServer},
        health_response, BufferState, BufferStateRequest, DeregisterSaeRequest, DrainRequest,
        HealthRequest, HealthResponse, ListSaesRequest, RegisterSaeRequest, RegisterSaeResponse,
        SaeInfo,
    },
};
use tonic::{transport::Server, Request, Response, Status};
use tracing::{info, instrument};

use crate::{agent_controller, service::DkmsService};

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
        let sae_id = m.sae_id.map(|s| s.value).unwrap_or_default();
        let id = agent_controller::register_sae(
            &self.svc,
            agent_controller::RegisterArgs {
                sae_id,
                rate_keys_per_sec: m.rate_keys_per_sec,
                burst_keys: m.burst_keys,
            },
        )
        .await
        .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(RegisterSaeResponse {
            registration_id: id,
            expires_at_unix_ms: chrono::Utc::now().timestamp_millis() + 24 * 3600 * 1000,
        }))
    }

    #[instrument(skip_all)]
    async fn deregister_sae(
        &self,
        req: Request<DeregisterSaeRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        let sae = req.into_inner().sae_id.map(|s| s.value).unwrap_or_default();
        agent_controller::deregister_sae(&self.svc, &sae)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn list_saes(
        &self,
        _req: Request<ListSaesRequest>,
    ) -> std::result::Result<Response<Self::ListSaesStream>, Status> {
        let (_tx, rx) = tokio::sync::mpsc::channel::<std::result::Result<SaeInfo, Status>>(16);
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    #[instrument(skip_all)]
    async fn drain(
        &self,
        req: Request<DrainRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        agent_controller::drain(&self.svc, req.into_inner().grace_seconds)
            .await
            .map_err(|e| Status::internal(e.to_string()))?;
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn health(
        &self,
        _req: Request<HealthRequest>,
    ) -> std::result::Result<Response<HealthResponse>, Status> {
        Ok(Response::new(HealthResponse { status: health_response::Status::Healthy as i32 }))
    }

    #[instrument(skip_all)]
    async fn get_buffer_state(
        &self,
        req: Request<BufferStateRequest>,
    ) -> std::result::Result<Response<BufferState>, Status> {
        let m = req.into_inner();
        let local  = m.local_sae.map(|s| s.value).unwrap_or_default();
        let remote = m.remote_sae.map(|s| s.value).unwrap_or_default();
        Ok(Response::new(BufferState {
            keys_available: self.svc.buffers.len(&local, &remote) as u64,
            keys_reserved:  0,
            bytes_buffered: 0,
            last_replenished_unix_ms: chrono::Utc::now().timestamp_millis(),
        }))
    }
}

pub async fn serve(svc: DkmsService, addr: &str) -> anyhow::Result<()> {
    let addr = addr.parse()?;
    info!(%addr, "dkms gRPC listening");
    Server::builder()
        .add_service(DkmsControlServer::new(DkmsGrpc { svc }))
        .serve(addr)
        .await?;
    Ok(())
}
