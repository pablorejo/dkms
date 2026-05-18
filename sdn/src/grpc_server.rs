use common::proto::{
    common::v1::{NodeId, Status as ProtoStatus},
    sdn::v1::{
        sdn_control_server::{SdnControl, SdnControlServer},
        AdmissionRequest, AdmissionResponse, CapacityReport, ComputePathRequest,
        ComputePathResponse, DkmsMetric, GetOrrPathRequest, GetOrrPathResponse,
        GetPathsWithRatiosRequest, GetPathsWithRatiosResponse, GetSaeBindingRequest,
        GetSaeBindingResponse, LinkUpdate, PathPolicy, PathWithRatio,
        StreamTopologyRequest, Topology as ProtoTopology, TopologyEvent,
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
        // FUERA DE ESCALCANCE: mutar la topología (añadir/quitar QKC,
        // ORR, DKMS, enlaces) en runtime no forma parte de este
        // proyecto. La topología se carga al boot desde
        // `cfg.topology_dir` (JSON) y vive inmutable hasta reiniciar
        // el SDN. Las únicas mutaciones soportadas son las del SAE
        // binding vía HTTP admin (`/sae`, `/sae/:id`). Ver CLAUDE.md
        // "Scope boundaries" para detalles.
        Err(Status::unimplemented(
            "PutTopology is out of scope: topology is loaded at boot from `topology_dir` \
             and immutable until SDN restart. Only SAE bindings are mutable at runtime \
             via HTTP admin (POST/PUT/DELETE /sae).",
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

    #[instrument(
        skip_all,
        fields(src = %req.get_ref().src_dkms, dst = %req.get_ref().dst_dkms)
    )]
    async fn get_paths_with_ratios(
        &self,
        req: Request<GetPathsWithRatiosRequest>,
    ) -> std::result::Result<Response<GetPathsWithRatiosResponse>, Status> {
        let m = req.into_inner();
        if m.src_dkms.is_empty() || m.dst_dkms.is_empty() {
            return Err(Status::invalid_argument("src_dkms and dst_dkms required"));
        }
        if m.src_dkms == m.dst_dkms {
            return Ok(Response::new(GetPathsWithRatiosResponse {
                paths: vec![],
                total_keys_per_second: 0.0,
            }));
        }

        let topo = self.svc.topology.load();
        // Construir el commodity para acceder a sus paths Yen's
        // filtrados — el orden de `Commodity.paths` es el que indexa
        // `McfSnapshot.rates_per_path`.
        let src_qkc = topo
            .qkc_of_dkms(&m.src_dkms)
            .ok_or_else(|| {
                Status::not_found(format!("dkms {} no anclado a un QKC", m.src_dkms))
            })?
            .to_string();
        let dst_qkc = topo
            .qkc_of_dkms(&m.dst_dkms)
            .ok_or_else(|| {
                Status::not_found(format!("dkms {} no anclado a un QKC", m.dst_dkms))
            })?
            .to_string();
        if src_qkc == dst_qkc {
            // Mismo QKC físico — no hay path inter-QKC, el caller debe
            // resolver localmente sin enviar por la red.
            return Ok(Response::new(GetPathsWithRatiosResponse {
                paths: vec![],
                total_keys_per_second: 0.0,
            }));
        }

        let raw = crate::mcf::k_shortest_paths(
            &topo.graph,
            &src_qkc,
            &dst_qkc,
            self.svc.solver.k_paths,
        );
        if raw.is_empty() {
            return Err(Status::not_found(format!(
                "no QKC path {src_qkc} → {dst_qkc}"
            )));
        }
        let filtered = crate::mcf::filter_overlapping_paths(
            &raw,
            crate::mcf::DEFAULT_OVERLAP_THRESHOLD,
        );

        let snap = self.svc.mcf_snapshot.load_full();
        let fid = crate::mcf::flow_id(&m.src_dkms, &m.dst_dkms);
        let total = snap.rates.get(&fid).copied().unwrap_or(0.0);
        let rpp = snap.rates_per_path.get(&fid).cloned().unwrap_or_default();

        let mut paths_out: Vec<PathWithRatio> = Vec::new();
        for (p_idx, p) in filtered.iter().enumerate() {
            let r_p = rpp.get(p_idx).copied().unwrap_or(0.0);
            if r_p <= 0.0 {
                continue;
            }
            let omega = if total > 0.0 { r_p / total } else { 0.0 };
            paths_out.push(PathWithRatio {
                qkc_hops: p.clone(),
                omega,
                keys_per_second: r_p,
            });
        }

        Ok(Response::new(GetPathsWithRatiosResponse {
            paths: paths_out,
            total_keys_per_second: total,
        }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::tests::make_service_for_test;

    #[tokio::test]
    async fn get_paths_with_ratios_returns_paths_after_recompute() {
        let svc = make_service_for_test();
        svc.recompute_mcf();
        let grpc = SdnGrpc { svc };

        let req = Request::new(GetPathsWithRatiosRequest {
            src_dkms: "dA".into(),
            dst_dkms: "dB".into(),
        });
        let resp = grpc.get_paths_with_ratios(req).await.unwrap().into_inner();
        // Hay rate (la pequeña topo de tests tiene capacidad > 0).
        assert!(resp.total_keys_per_second > 0.0);
        // Y al menos un path en el output.
        assert!(!resp.paths.is_empty(), "se espera al menos un path");
        // Σ omega ≈ 1.0 ± EPSILON.
        let omega_sum: f64 = resp.paths.iter().map(|p| p.omega).sum();
        assert!(
            (omega_sum - 1.0).abs() < 1e-6,
            "Σ omega debe ser 1.0; got {omega_sum}",
        );
        // Σ keys_per_second ≈ total_keys_per_second.
        let kps_sum: f64 = resp.paths.iter().map(|p| p.keys_per_second).sum();
        assert!((kps_sum - resp.total_keys_per_second).abs() < 1e-6);
    }

    #[tokio::test]
    async fn get_paths_with_ratios_empty_on_same_src_dst() {
        let svc = make_service_for_test();
        let grpc = SdnGrpc { svc };
        let req = Request::new(GetPathsWithRatiosRequest {
            src_dkms: "dA".into(),
            dst_dkms: "dA".into(),
        });
        let resp = grpc.get_paths_with_ratios(req).await.unwrap().into_inner();
        assert!(resp.paths.is_empty());
        assert_eq!(resp.total_keys_per_second, 0.0);
    }

    #[tokio::test]
    async fn get_paths_with_ratios_rejects_empty_args() {
        let svc = make_service_for_test();
        let grpc = SdnGrpc { svc };
        let req = Request::new(GetPathsWithRatiosRequest {
            src_dkms: "".into(),
            dst_dkms: "dB".into(),
        });
        let err = grpc.get_paths_with_ratios(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_paths_with_ratios_404_on_unknown_dkms() {
        let svc = make_service_for_test();
        let grpc = SdnGrpc { svc };
        let req = Request::new(GetPathsWithRatiosRequest {
            src_dkms: "dXX".into(),
            dst_dkms: "dB".into(),
        });
        let err = grpc.get_paths_with_ratios(req).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
