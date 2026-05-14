//! gRPC server implementing the `QkcControl` service.
//!
//! All RPCs delegate to [`QkcService`]; the goal here is just to translate
//! between protobuf messages and the service-layer types.

use common::proto::qkc::v1::{
    qkc_control_server::{QkcControl, QkcControlServer},
    BucketUpdate, CapacityReport, KmeState, KmeStateRequest, ReleaseRequest, ReserveRequest,
    ReserveResponse,
};
use common::proto::common::v1::Status as ProtoStatus;
use tokio::sync::mpsc;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{info, instrument};

use crate::service::QkcService;

pub struct QkcGrpc {
    pub svc: QkcService,
}

#[tonic::async_trait]
impl QkcControl for QkcGrpc {
    type StreamKmeStateStream =
        tokio_stream::wrappers::ReceiverStream<std::result::Result<KmeState, Status>>;

    #[instrument(skip_all)]
    async fn push_capacity(
        &self,
        _req: Request<CapacityReport>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        // TODO: forward into QkcService::buckets / metrics
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn update_bucket(
        &self,
        req: Request<BucketUpdate>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        let m = req.into_inner();
        if let Some(link) = m.link {
            self.svc.buckets.update(&link.value, m.capacity_keys, m.refill_rate_keys_per_sec);
        }
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn get_kme_state(
        &self,
        req: Request<KmeStateRequest>,
    ) -> std::result::Result<Response<KmeState>, Status> {
        let m = req.into_inner();
        let peer = m.peer.map(|p| p.value).unwrap_or_default();
        Ok(Response::new(KmeState {
            peer: Some(common::proto::common::v1::NodeId { value: peer.clone() }),
            keys_available: self.svc.kme.available(&peer),
            keys_reserved: 0,
            last_replenished_unix_ms: chrono::Utc::now().timestamp_millis(),
        }))
    }

    #[instrument(skip_all)]
    async fn reserve(
        &self,
        _req: Request<ReserveRequest>,
    ) -> std::result::Result<Response<ReserveResponse>, Status> {
        // TODO: take(count) from KME, register reservation, return key_ids
        Err(Status::unimplemented("reserve"))
    }

    #[instrument(skip_all)]
    async fn release(
        &self,
        _req: Request<ReleaseRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn stream_kme_state(
        &self,
        _req: Request<KmeStateRequest>,
    ) -> std::result::Result<Response<Self::StreamKmeStateStream>, Status> {
        let (_tx, rx) = mpsc::channel::<std::result::Result<KmeState, Status>>(16);
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

pub async fn serve(svc: QkcService, addr: &str) -> anyhow::Result<()> {
    let addr = addr.parse()?;
    info!(%addr, "qkc gRPC listening");
    Server::builder()
        .add_service(QkcControlServer::new(QkcGrpc { svc }))
        .serve(addr)
        .await?;
    Ok(())
}
