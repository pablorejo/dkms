use common::proto::{
    common::v1::{NodeId, Status as ProtoStatus},
    sdn::v1::{
        sdn_control_server::{SdnControl, SdnControlServer},
        AdmissionRequest, AdmissionResponse, CapacityReport, ComputePathRequest,
        ComputePathResponse, DkmsMetric, GetOrrPathRequest, GetOrrPathResponse,
        GetSaeBindingRequest, GetSaeBindingResponse, LinkUpdate, PathPolicy, StreamTopologyRequest,
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
        // La topología SÍ muta en runtime, pero no empujando un grafo
        // entero: cada módulo se declara a sí mismo por el HTTP admin
        // (`POST /register/{qkc,orr,dkms}`) y el SDN la infiere. Un
        // `PutTopology` sería una segunda fuente de verdad que además
        // no podría expirar nodos. Ver CLAUDE.md, "Topology is
        // inferred, never configured".
        Err(Status::unimplemented(
            "PutTopology is out of scope: the SDN infers its topology from the modules' \
             own announcements (POST /register/{qkc,orr,dkms} on the HTTP admin). There \
             is no whole-graph push.",
        ))
    }

    #[instrument(skip_all)]
    async fn update_link(
        &self,
        _req: Request<LinkUpdate>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        // FUERA DE ALCANCE: ver `put_topology`. La capacidad de un
        // enlace SÍ se puede mutar, pero por el HTTP admin endpoint
        // `POST /link-capacity` (no por gRPC).
        Err(Status::unimplemented(
            "UpdateLink is out of scope. For link-capacity updates use HTTP admin \
             `POST /link-capacity`. Topology mutations (links, nodes) are not \
             supported at runtime.",
        ))
    }

    #[instrument(skip_all)]
    async fn compute_path(
        &self,
        req: Request<ComputePathRequest>,
    ) -> std::result::Result<Response<ComputePathResponse>, Status> {
        let m = req.into_inner();
        let src = m.src.unwrap_or_default().value;
        let dst = m.dst.unwrap_or_default().value;
        let policy = match PathPolicy::try_from(m.policy).unwrap_or(PathPolicy::PolicyUnspecified) {
            PathPolicy::ShortestHops => routing::Policy::ShortestHops,
            PathPolicy::MinLatency => routing::Policy::MinLatency,
            PathPolicy::MaxAvailableCapacity => routing::Policy::MaxAvailableCapacity,
            PathPolicy::MinCostFlow => routing::Policy::MinCostFlow,
            PathPolicy::PolicyUnspecified => routing::Policy::ShortestHops,
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
            allowed: d.allowed,
            granted_bps: d.granted_bps,
            reason: d.reason,
        }))
    }

    #[instrument(skip_all)]
    async fn stream_topology(
        &self,
        _req: Request<StreamTopologyRequest>,
    ) -> std::result::Result<Response<Self::StreamTopologyStream>, Status> {
        let rx = self.svc.pushers.subscribe();
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
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

    #[instrument(skip_all, fields(sae = %req.get_ref().sae_id))]
    async fn get_sae_binding(
        &self,
        req: Request<GetSaeBindingRequest>,
    ) -> std::result::Result<Response<GetSaeBindingResponse>, Status> {
        let sae_id = req.into_inner().sae_id;
        if sae_id.is_empty() {
            return Err(Status::invalid_argument("sae_id required"));
        }
        let topo = self.svc.topology.load();
        let sae = topo
            .saes
            .get(&sae_id)
            .ok_or_else(|| Status::not_found(format!("sae {sae_id} not registered")))?;
        let dkms = topo
            .dkms
            .get(&sae.dkms_id)
            .ok_or_else(|| Status::failed_precondition(format!("dkms {} unknown", sae.dkms_id)))?;
        Ok(Response::new(GetSaeBindingResponse {
            dkms_id: dkms.id.clone(),
            orr_id: dkms.orr_id.clone(),
        }))
    }

    #[instrument(skip_all, fields(src = %req.get_ref().src_orr, dst = %req.get_ref().dst_orr))]
    async fn get_orr_path(
        &self,
        req: Request<GetOrrPathRequest>,
    ) -> std::result::Result<Response<GetOrrPathResponse>, Status> {
        let m = req.into_inner();
        if m.src_orr.is_empty() || m.dst_orr.is_empty() {
            return Err(Status::invalid_argument("src_orr and dst_orr required"));
        }
        if m.src_orr == m.dst_orr {
            return Ok(Response::new(GetOrrPathResponse {
                orrs: vec![m.src_orr],
            }));
        }
        let topo = self.svc.topology.load();
        // ORR-level path = mapear cada orr → su qkc, computar shortest
        // QKC path, mapear cada qkc del path de vuelta a su orr.
        let src_orr = topo
            .orrs
            .get(&m.src_orr)
            .ok_or_else(|| Status::not_found(format!("orr {} unknown", m.src_orr)))?;
        let dst_orr = topo
            .orrs
            .get(&m.dst_orr)
            .ok_or_else(|| Status::not_found(format!("orr {} unknown", m.dst_orr)))?;
        let qkc_path = topo
            .shortest_path_qkc(&src_orr.qkc_id, &dst_orr.qkc_id)
            .ok_or_else(|| {
                Status::not_found(format!(
                    "no qkc path {} → {}",
                    src_orr.qkc_id, dst_orr.qkc_id
                ))
            })?;
        // En la estrella, los QKC hub/intermedios pueden no tener un
        // ORR adjunto (sólo las hojas lo tienen). El camino ORR-level
        // sólo lista los QKCs que SÍ tienen ORR; si entre src y dst
        // hay un único hop QKC sin ORRs intermedios, el path resultante
        // es [src_orr, dst_orr] y el ORR del origen lo interpretará
        // como modo PQC E2E (1 capa).
        let orrs: Vec<String> = qkc_path
            .iter()
            .filter_map(|qkc| topo.orr_by_qkc.get(qkc).cloned())
            .collect();
        Ok(Response::new(GetOrrPathResponse { orrs }))
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
