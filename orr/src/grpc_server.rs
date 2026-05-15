//! Servidor gRPC del ORR (`OrrControl`).
//!
//! Dos superficies:
//!
//!   * **Higher-layer** (lo que usa el DKMS):
//!     `SendMessage` — empuja un payload hacia un ORR destino.
//!     `StreamDeliveries` — el DKMS se suscribe y recibe lo que el
//!     QKC local entrega para este nodo.
//!
//!   * **Onion-circuit** (`OpenCircuit` / `Relay` / …): stubs. La
//!     cebolla PQC capa-a-capa todavía no está cableada — devuelven
//!     `UNIMPLEMENTED` para que el caller falle limpio en lugar de
//!     pensar que funcionó.
//!
//! Con `max_hops = 0` (passthrough) toda la superficie superior es
//! suficiente y no hace falta tocar nada del bloque onion.

use common::proto::common::v1::{NodeId, Status as ProtoStatus};
use common::proto::orr::v1::{
    orr_control_server::{OrrControl, OrrControlServer},
    Circuit as ProtoCircuit, CloseCircuitRequest, DeliveredMessage, EstablishSecretRequest,
    EstablishSecretResponse, GetCircuitRequest, GetPublicKeyRequest, GetPublicKeyResponse,
    ListCircuitsRequest, OpenCircuitRequest, OpenCircuitResponse, RelayFrame,
    SendMessageRequest, SendMessageResponse, StreamDeliveriesRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use crate::{error::OrrError, service::OrrService};

pub struct OrrGrpc {
    pub svc: OrrService,
}

#[tonic::async_trait]
impl OrrControl for OrrGrpc {
    type ListCircuitsStream = ReceiverStream<std::result::Result<ProtoCircuit, Status>>;
    type StreamDeliveriesStream = ReceiverStream<std::result::Result<DeliveredMessage, Status>>;

    // ── higher-layer surface ───────────────────────────────────────────
    #[instrument(skip_all, fields(dest, max_hops))]
    async fn send_message(
        &self,
        req: Request<SendMessageRequest>,
    ) -> std::result::Result<Response<SendMessageResponse>, Status> {
        let m = req.into_inner();
        let dest = m
            .destination
            .as_ref()
            .map(|n| n.value.clone())
            .unwrap_or_default();
        if dest.is_empty() {
            return Err(Status::invalid_argument("destination requerido"));
        }
        // El proto distingue "no establecido" vía `has_max_hops` para
        // que `0` siga siendo un valor válido (passthrough) y no se
        // confunda con default.
        let max_hops = if m.has_max_hops {
            m.max_hops
        } else {
            self.svc.cfg.default_max_hops
        };
        let app_header: std::collections::BTreeMap<String, String> =
            m.app_header.into_iter().collect();

        tracing::Span::current().record("dest", tracing::field::display(&dest));
        tracing::Span::current().record("max_hops", max_hops);

        let outcome = self
            .svc
            .send_message(&dest, m.payload, max_hops, app_header)
            .await
            .map_err(map_err)?;

        Ok(Response::new(SendMessageResponse {
            status: outcome.status.to_string(),
            final_destination: Some(NodeId { value: outcome.final_dest_orr }),
            next_hop_qkc: outcome.next_hop_qkc,
            remaining_hops: outcome.remaining_hops,
            pqc_layer: outcome.pqc_layer,
        }))
    }

    #[instrument(skip_all, fields(subscriber))]
    async fn stream_deliveries(
        &self,
        req: Request<StreamDeliveriesRequest>,
    ) -> std::result::Result<Response<Self::StreamDeliveriesStream>, Status> {
        let sub_id = req.into_inner().subscriber_id;
        tracing::Span::current().record("subscriber", tracing::field::display(&sub_id));
        info!(subscriber = %sub_id, "orr.stream_deliveries subscribed");

        let mut rx = self.svc.subscribe_deliveries();
        let (tx, out_rx) = mpsc::channel::<std::result::Result<DeliveredMessage, Status>>(
            self.svc.cfg.deliver_queue_capacity.max(1),
        );

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(msg) => {
                        if tx.send(Ok(msg)).await.is_err() {
                            debug!(subscriber = %sub_id, "orr.stream_deliveries client gone");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!(subscriber = %sub_id, lost = n, "orr.stream_deliveries lagged");
                        // No abortamos: seguimos sirviendo lo siguiente
                        // (semántica lossy intencional).
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(out_rx)))
    }

    #[instrument(skip_all)]
    async fn get_public_key(
        &self,
        _req: Request<GetPublicKeyRequest>,
    ) -> std::result::Result<Response<GetPublicKeyResponse>, Status> {
        let id = &self.svc.identity;
        Ok(Response::new(GetPublicKeyResponse {
            public_key: id.public_key.clone(),
            suite:      id.suite.clone(),
            orr_id:     Some(NodeId { value: self.svc.cfg.orr_id.clone() }),
        }))
    }

    #[instrument(skip_all, fields(from))]
    async fn establish_secret(
        &self,
        req: Request<EstablishSecretRequest>,
    ) -> std::result::Result<Response<EstablishSecretResponse>, Status> {
        let m = req.into_inner();
        let from = m
            .from
            .as_ref()
            .map(|n| n.value.to_lowercase())
            .unwrap_or_default();
        if from.is_empty() {
            return Ok(Response::new(EstablishSecretResponse {
                ok:    false,
                error: "from field required".into(),
            }));
        }
        tracing::Span::current().record("from", tracing::field::display(&from));

        // Idempotente: si ya teníamos master_secret con este peer, no
        // sobreescribimos (puede ser un reintento del initiator). Aún
        // así devolvemos ok para que el initiator no haga loop.
        if self.svc.peers.has_master_secret(&from) {
            debug!(peer = %from, "orr.establish_secret already_set idempotent_ok");
            return Ok(Response::new(EstablishSecretResponse {
                ok:    true,
                error: String::new(),
            }));
        }

        // Decapsular con nuestra sk.
        let ss = match self.svc.identity.decap(&m.ciphertext) {
            Ok(ss) => ss,
            Err(e) => {
                warn!(peer = %from, error = %e, "orr.establish_secret decap_failed");
                return Ok(Response::new(EstablishSecretResponse {
                    ok:    false,
                    error: format!("decap: {e}"),
                }));
            }
        };
        if ss.len() != 32 {
            return Ok(Response::new(EstablishSecretResponse {
                ok:    false,
                error: format!("shared_secret unexpected len {}", ss.len()),
            }));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&ss);
        self.svc.peers.put_master_secret(from.clone(), secret);
        info!(peer = %from, "orr.establish_secret stored");
        Ok(Response::new(EstablishSecretResponse {
            ok:    true,
            error: String::new(),
        }))
    }

    // ── onion-circuit surface (stubs) ──────────────────────────────────
    #[instrument(skip_all)]
    async fn open_circuit(
        &self,
        _req: Request<OpenCircuitRequest>,
    ) -> std::result::Result<Response<OpenCircuitResponse>, Status> {
        Err(Status::unimplemented("onion circuits not wired yet"))
    }

    #[instrument(skip_all)]
    async fn close_circuit(
        &self,
        _req: Request<CloseCircuitRequest>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        Err(Status::unimplemented("onion circuits not wired yet"))
    }

    #[instrument(skip_all)]
    async fn get_circuit(
        &self,
        _req: Request<GetCircuitRequest>,
    ) -> std::result::Result<Response<ProtoCircuit>, Status> {
        Err(Status::unimplemented("onion circuits not wired yet"))
    }

    #[instrument(skip_all)]
    async fn list_circuits(
        &self,
        _req: Request<ListCircuitsRequest>,
    ) -> std::result::Result<Response<Self::ListCircuitsStream>, Status> {
        Err(Status::unimplemented("onion circuits not wired yet"))
    }

    #[instrument(skip_all)]
    async fn relay(
        &self,
        _req: Request<RelayFrame>,
    ) -> std::result::Result<Response<ProtoStatus>, Status> {
        Err(Status::unimplemented("onion relay not wired yet"))
    }
}

fn map_err(e: OrrError) -> Status {
    match e {
        OrrError::NotFound(s) => Status::not_found(s),
        OrrError::Unsupported(s) => Status::unimplemented(s),
        OrrError::InvalidPath(s) => Status::invalid_argument(s),
        OrrError::Relay(s) => Status::failed_precondition(s),
        other => Status::internal(other.to_string()),
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
