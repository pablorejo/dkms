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
    Circuit as ProtoCircuit, CloseCircuitRequest, DeliveredMessage,
    EstablishEphemeralSecretRequest, EstablishEphemeralSecretResponse, EstablishSecretRequest,
    EstablishSecretResponse, GetCircuitRequest, GetPublicKeyRequest, GetPublicKeyResponse,
    ListCircuitsRequest, OpenCircuitRequest, OpenCircuitResponse, RelayFrame,
    RequestEphemeralKeyRequest, RequestEphemeralKeyResponse, SendMessageRequest,
    SendMessageResponse, StreamDeliveriesRequest,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{debug, info, instrument, warn};

use crate::{error::OrrError, service::OrrService};

pub struct OrrGrpc {
    pub svc: OrrService,
}

/// El `from` del cuerpo tiene que ser la identidad del certificado mTLS del
/// que llama (SAN `dkms://<id>`). Sin esto, cualquier miembro de la red —un
/// DKMS de otra institución con su cert de `net-ca`— podía reclamar ser
/// `orr_X` y sobrescribirle el `bootstrap_secret` en este ORR: el mTLS solo
/// probaba «alguien de la red», no «quien dice ser». Sin certs (`grpc_tls =
/// false`, red interna de confianza) se deja pasar y se avisa una vez.
fn require_from_matches_peer(
    peer_certs: Option<std::sync::Arc<Vec<tonic::transport::CertificateDer<'static>>>>,
    from: &str,
) -> std::result::Result<(), Status> {
    match peer_certs {
        Some(certs) => {
            let claimed = from.to_ascii_lowercase();
            match common::cert_identity::node_id_from_certs(&certs) {
                Some(id) if id == claimed => Ok(()),
                other => {
                    warn!(
                        from = %claimed,
                        cert_identity = ?other,
                        "orr.grpc: el `from` del cuerpo no coincide con la identidad del cert \
                         mTLS del que llama; rechazado"
                    );
                    Err(Status::permission_denied(format!(
                        "from={claimed} no coincide con la identidad del certificado ({other:?})"
                    )))
                }
            }
        }
        None => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                warn!(
                    "orr.grpc: llamada sin certificado de cliente (grpc_tls = false): el `from` \
                     de EstablishSecret y de las rotaciones va SIN autenticar"
                );
            });
            Ok(())
        }
    }
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
        // Key grade for the relayed frame (0 = QKD, 1 = PQC). Clamp unknown
        // proto values to QKD (the safe default).
        let grade = if m.grade == u32::from(wire::GRADE_PQC) {
            wire::GRADE_PQC
        } else {
            wire::GRADE_QKD
        };

        tracing::Span::current().record("dest", tracing::field::display(&dest));
        tracing::Span::current().record("max_hops", max_hops);

        let outcome = self
            .svc
            .send_message(&dest, m.payload, max_hops, app_header, grade)
            .await
            .map_err(map_err)?;

        Ok(Response::new(SendMessageResponse {
            status: outcome.status.to_string(),
            final_destination: Some(NodeId {
                value: outcome.final_dest_orr,
            }),
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
        // Firma ML-DSA del anuncio (§Fase 6 PQC), con la cadena del cert de
        // nodo si se firma con él; vacías si este ORR no firma.
        let (signature, signing_certs) = id.sign_pubkey_announcement().unwrap_or_default();
        Ok(Response::new(GetPublicKeyResponse {
            public_key: id.public_key.clone(),
            suite: id.suite.clone(),
            orr_id: Some(NodeId {
                value: self.svc.cfg.orr_id.clone(),
            }),
            signature,
            signing_certs,
        }))
    }

    #[instrument(skip_all, fields(from))]
    async fn establish_secret(
        &self,
        req: Request<EstablishSecretRequest>,
    ) -> std::result::Result<Response<EstablishSecretResponse>, Status> {
        let peer_certs = req.peer_certs();
        let m = req.into_inner();
        let from = m
            .from
            .as_ref()
            .map(|n| n.value.to_lowercase())
            .unwrap_or_default();
        if from.is_empty() {
            return Ok(Response::new(EstablishSecretResponse {
                ok: false,
                error: "from field required".into(),
            }));
        }
        tracing::Span::current().record("from", tracing::field::display(&from));
        require_from_matches_peer(peer_certs, &from)?;

        // NOTA passive re-bootstrap: anteriormente había aquí un early
        // return `if has_bootstrap → ok` por idempotencia. Eso rompía la
        // auto-cura cuando el initiator se reiniciaba: re-mandaba un
        // EstablishSecret con NUEVO ciphertext y el responder lo
        // ignoraba silenciosamente, dejando los dos lados con
        // master_secrets distintos para siempre. Ahora SIEMPRE
        // decapsulamos y sobrescribimos. El caso "mismo encap repetido"
        // sigue siendo idempotente porque decap(ct) es determinista —
        // sobreescribir con el mismo valor no cambia nada.

        // Decapsular con nuestra sk.
        let ss = match self.svc.identity.decap(&m.ciphertext) {
            Ok(ss) => ss,
            Err(e) => {
                warn!(peer = %from, error = %e, "orr.establish_secret decap_failed");
                return Ok(Response::new(EstablishSecretResponse {
                    ok: false,
                    error: format!("decap: {e}"),
                }));
            }
        };
        if ss.len() != 32 {
            return Ok(Response::new(EstablishSecretResponse {
                ok: false,
                error: format!("shared_secret unexpected len {}", ss.len()),
            }));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&ss);
        // El shared_secret del bootstrap es la clave HMAC de las rotaciones
        // (`RequestEphemeralKey` + `EstablishEphemeralSecret`, keypair
        // efímera por época) y siembra la época 0. Quien nos manda un
        // EstablishSecret ha rehecho su bootstrap —se reinició, o vio que
        // nos reiniciamos—, así que su historia con nosotros ya no existe:
        // la nuestra con él tampoco puede seguir, o le cifraríamos con
        // épocas que no tiene (`reset_for_bootstrap`).
        self.svc.peers.reset_for_bootstrap(&from, secret);
        info!(peer = %from, "orr.establish_secret bootstrap_secret stored (historia de épocas a cero)");
        Ok(Response::new(EstablishSecretResponse {
            ok: true,
            error: String::new(),
        }))
    }

    // ── ORR Option-B rotation handlers (lado responder) ────────────────
    //
    // Delegamos a las funciones puras en `rotation.rs` para que la
    // lógica esté en un único sitio y los tests in-process (OBJ-014)
    // puedan reutilizarla byte-a-byte. Aquí sólo desempaquetamos el
    // `Request<...>`, hacemos record de los campos al span de tracing,
    // y volvemos a empaquetar en `Response<...>`.
    #[instrument(skip_all, fields(from, epoch))]
    async fn request_ephemeral_key(
        &self,
        req: Request<RequestEphemeralKeyRequest>,
    ) -> std::result::Result<Response<RequestEphemeralKeyResponse>, Status> {
        let peer_certs = req.peer_certs();
        let m = req.into_inner();
        tracing::Span::current().record(
            "from",
            tracing::field::display(m.from.as_ref().map(|n| n.value.as_str()).unwrap_or("")),
        );
        tracing::Span::current().record("epoch", m.epoch_id);
        let claimed = m
            .from
            .as_ref()
            .map(|n| n.value.to_lowercase())
            .unwrap_or_default();
        require_from_matches_peer(peer_certs, &claimed)?;
        let resp = crate::rotation::handle_request_ephemeral_key(
            &self.svc.peers,
            &self.svc.cfg.orr_id,
            &self.svc.cfg.default_pqc_suite,
            m,
        );
        Ok(Response::new(resp))
    }

    #[instrument(skip_all, fields(from, epoch))]
    async fn establish_ephemeral_secret(
        &self,
        req: Request<EstablishEphemeralSecretRequest>,
    ) -> std::result::Result<Response<EstablishEphemeralSecretResponse>, Status> {
        let peer_certs = req.peer_certs();
        let m = req.into_inner();
        tracing::Span::current().record(
            "from",
            tracing::field::display(m.from.as_ref().map(|n| n.value.as_str()).unwrap_or("")),
        );
        tracing::Span::current().record("epoch", m.epoch_id);
        let claimed = m
            .from
            .as_ref()
            .map(|n| n.value.to_lowercase())
            .unwrap_or_default();
        require_from_matches_peer(peer_certs, &claimed)?;
        let resp = crate::rotation::handle_establish_ephemeral_secret(
            &self.svc.peers,
            &self.svc.cfg.orr_id,
            &self.svc.cfg.default_pqc_suite,
            self.svc.cfg.epoch_history_keep,
            m,
        );
        Ok(Response::new(resp))
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

pub async fn serve(svc: OrrService, addr: &str, mtls: bool) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = addr.parse()?;
    let listener = common::net::bind_reuse_addr(addr).await?;
    let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
    let mut builder = Server::builder();
    if mtls {
        // Identidad de nodo + cert de cliente obligatorio de la CA de red:
        // cubre al DKMS que nos habla y a los ORR pares. Ver `grpc_tls`.
        let tls = crate::grpc_tls::server()?
            .ok_or_else(|| anyhow::anyhow!("grpc_tls = true pero no hay [tls] configurado"))?;
        builder = builder.tls_config(tls)?;
        info!(%addr, "orr gRPC listening (mTLS)");
    } else {
        info!(%addr, "orr gRPC listening (plaintext)");
    }
    builder
        .add_service(OrrControlServer::new(OrrGrpc { svc }))
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}
