use common::proto::orr::v1::{
    orr_control_server::{OrrControl, OrrControlServer},
    Circuit as ProtoCircuit, CircuitState as ProtoCircuitState, CloseCircuitRequest,
    GetCircuitRequest, ListCircuitsRequest, OpenCircuitRequest, OpenCircuitResponse, RelayFrame,
};
use common::proto::common::v1::Status as ProtoStatus;
use tokio::sync::mpsc;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{info, instrument};
use uuid::Uuid;

use crate::{
    relay::{Circuit, CircuitState},
    service::OrrService,
};

pub struct OrrGrpc {
    pub svc: OrrService,
}

#[tonic::async_trait]
impl OrrControl for OrrGrpc {
    type ListCircuitsStream =
        tokio_stream::wrappers::ReceiverStream<std::result::Result<ProtoCircuit, Status>>;

    #[instrument(skip_all)]
    async fn open_circuit(
        &self,
        req: Request<OpenCircuitRequest>,
    ) -> std::result::Result<Response<OpenCircuitResponse>, Status> {
        let m = req.into_inner();
        let id = if m.circuit_id.is_empty() { Uuid::new_v4().to_string() } else { m.circuit_id };
        let circuit = Circuit {
            id: id.clone(),
            path: m.path.into_iter().map(|n| n.value).collect(),
            session_keys: std::sync::Arc::new(vec![]),
            state: CircuitState::Open,
            opened_at: chrono::Utc::now(),
            last_used: chrono::Utc::now(),
            frames: 0,
        };
        self.svc.circuits.insert(circuit);
        Ok(Response::new(OpenCircuitResponse {
            circuit_id: id,
            session_id: vec![],
        }))
    }

    #[instrument(skip_all)]
    async fn close_circuit(
        &self,
        req: Request<CloseCircuitRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        self.svc.circuits.remove(&req.into_inner().circuit_id);
        Ok(Response::new(ProtoStatus::default()))
    }

    #[instrument(skip_all)]
    async fn get_circuit(
        &self,
        req: Request<GetCircuitRequest>,
    ) -> std::result::Result<Response<ProtoCircuit>, Status> {
        let id = req.into_inner().circuit_id;
        let c = self.svc.circuits.get(&id).ok_or_else(|| Status::not_found(id))?;
        Ok(Response::new(to_proto(&c)))
    }

    #[instrument(skip_all)]
    async fn list_circuits(
        &self,
        _req: Request<ListCircuitsRequest>,
    ) -> std::result::Result<Response<Self::ListCircuitsStream>, Status> {
        let circuits: Vec<_> = self.svc.circuits.iter().collect();
        let (tx, rx) = mpsc::channel(circuits.len().max(1));
        for c in circuits {
            let _ = tx.send(Ok(to_proto(&c))).await;
        }
        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    #[instrument(skip_all)]
    async fn relay(
        &self,
        req: Request<RelayFrame>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        let m = req.into_inner();
        self.svc.circuits.touch(&m.circuit_id);
        // TODO: peel onion layer, forward to next hop using `self.svc.peers`.
        Ok(Response::new(ProtoStatus::default()))
    }
}

fn to_proto(c: &Circuit) -> ProtoCircuit {
    ProtoCircuit {
        circuit_id: c.id.clone(),
        path: c
            .path
            .iter()
            .cloned()
            .map(|v| common::proto::common::v1::NodeId { value: v })
            .collect(),
        state: match c.state {
            CircuitState::Opening => ProtoCircuitState::Opening,
            CircuitState::Open    => ProtoCircuitState::Open,
            CircuitState::Closing => ProtoCircuitState::Closing,
            CircuitState::Closed  => ProtoCircuitState::Closed,
            CircuitState::Failed  => ProtoCircuitState::Failed,
        } as i32,
        opened_at_unix_ms: c.opened_at.timestamp_millis(),
        last_used_unix_ms: c.last_used.timestamp_millis(),
        frames_relayed: c.frames,
    }
}

pub async fn serve(svc: OrrService, addr: &str) -> anyhow::Result<()> {
    let addr = addr.parse()?;
    info!(%addr, "orr gRPC listening");
    Server::builder()
        .add_service(OrrControlServer::new(OrrGrpc { svc }))
        .serve(addr)
        .await?;
    Ok(())
}
