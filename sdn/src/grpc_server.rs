use common::proto::{
    common::v1::{NodeId, Status as ProtoStatus},
    sdn::v1::{
        sdn_control_server::{SdnControl, SdnControlServer},
        AdmissionRequest, AdmissionResponse, CapacityReport, ComputePathRequest,
        ComputePathResponse, DkmsMetric, LinkUpdate, PathPolicy, StreamTopologyRequest,
        Topology as ProtoTopology, TopologyEvent,
    },
};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tracing::{info, instrument};

use crate::{routing, service::SdnService};

pub struct SdnGrpc {
    pub svc: SdnService,
}

#[tonic::async_trait]
impl SdnControl for SdnGrpc {
    type StreamTopologyStream =
        tokio_stream::wrappers::ReceiverStream<std::result::Result<TopologyEvent, Status>>;

    #[instrument(skip_all)]
    async fn put_topology(
        &self,
        _req: Request<ProtoTopology>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        // TODO: parse the proto, build a fresh Topology, swap it in.
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn update_link(
        &self,
        _req: Request<LinkUpdate>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn compute_path(
        &self,
        req: Request<ComputePathRequest>,
    ) -> std::result::Result<Response<ComputePathResponse>, Status> {
        let m = req.into_inner();
        let src = m.src.unwrap_or_default().value;
        let dst = m.dst.unwrap_or_default().value;
        let policy = match PathPolicy::try_from(m.policy).unwrap_or(PathPolicy::Unspecified) {
            PathPolicy::ShortestHops          => routing::Policy::ShortestHops,
            PathPolicy::MinLatency            => routing::Policy::MinLatency,
            PathPolicy::MaxAvailableCapacity  => routing::Policy::MaxAvailableCapacity,
            PathPolicy::MinCostFlow           => routing::Policy::MinCostFlow,
            PathPolicy::Unspecified           => routing::Policy::ShortestHops,
        };
        let p = routing::compute(&self.svc.topology, &src, &dst, m.required_bps, policy)
            .map_err(|e| Status::not_found(e.to_string()))?;
        Ok(Response::new(ComputePathResponse {
            path: p.nodes.into_iter().map(|v| NodeId { value: v }).collect(),
            estimated_latency_us: p.estimated_latency_us,
            bottleneck_capacity_bps: p.bottleneck_capacity_bps,
        }))
    }

    #[instrument(skip_all)]
    async fn check_admission(
        &self,
        req: Request<AdmissionRequest>,
    ) -> std::result::Result<Response<AdmissionResponse>, Status> {
        let m = req.into_inner();
        let link = m.link.unwrap_or_default().value;
        let d = crate::link_admission::check(&self.svc.topology, &link, m.requested_bps);
        Ok(Response::new(AdmissionResponse {
            allowed:    d.allowed,
            granted_bps: d.granted_bps,
            reason:     d.reason,
        }))
    }

    #[instrument(skip_all)]
    async fn stream_topology(
        &self,
        _req: Request<StreamTopologyRequest>,
    ) -> std::result::Result<Response<Self::StreamTopologyStream>, Status> {
        let rx = self.svc.pushers.subscribe();
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    #[instrument(skip_all)]
    async fn report_capacity(
        &self,
        _req: Request<Streaming<CapacityReport>>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        // TODO: consume the stream, feed into link_admission state.
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn report_dkms_metrics(
        &self,
        _req: Request<Streaming<DkmsMetric>>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        // TODO: feed into MCF demands.
        Ok(Response::new(ProtoStatus::default()))
    }
}

pub async fn serve(svc: SdnService, addr: &str) -> anyhow::Result<()> {
    let addr = addr.parse()?;
    info!(%addr, "sdn gRPC listening");
    Server::builder()
        .add_service(SdnControlServer::new(SdnGrpc { svc }))
        .serve(addr)
        .await?;
    Ok(())
}
