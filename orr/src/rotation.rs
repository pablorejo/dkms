//! ORR Option-B forward secrecy: cliente de rotación de master_secret.
//!
//! El bootstrap inicial (`bootstrap.rs`) deriva un `bootstrap_secret`
//! por par mediante ML-KEM encap contra la long-term pubkey del peer.
//! **Este `bootstrap_secret` sólo se usa como clave HMAC** para
//! autenticar las RPCs de rotación; jamás como keystream del onion.
//!
//! Cada `rotation_period_ms` (config), una task por peer (sólo en el
//! lado lex-smaller = initiator) ejecuta una rotación que produce un
//! `master_secret` nuevo con una keypair ML-KEM-768 **efímera fresca**:
//!
//! ```text
//!   A (initiator, lex-smaller)             B (responder)
//!   epoch = current_send_epoch[B] + 1
//!   mac1 = mac_req(bootstrap_AB, epoch, A, B)
//!   ──── RequestEphemeralKey(epoch, mac1) ────►
//!                                          verify_mac_req
//!                                          (epk, esk) ← ML-KEM-768 keygen
//!                                          ephemeral_sks[A][epoch] = esk
//!                                          mac2 = mac_resp(bootstrap_AB,
//!                                                  epoch, A, B, epk)
//!   ◄──────── { ok, epk, mac2 } ──────────
//!   verify_mac_resp
//!   (ct, ss) = encap(epk)
//!   master_secrets[B][epoch] = ss
//!   mac3 = mac_fin(bootstrap_AB, epoch, A, B, ct)
//!   ─── EstablishEphemeralSecret(epoch, ct, mac3) ──►
//!                                          verify_mac_fin
//!                                          esk = take_ephemeral_sk(A, epoch)
//!                                          ss = decap(esk, ct)
//!                                          master_secrets[A][epoch] = ss
//!                                          drop(esk) // Zeroizing → wipe
//!                                          ┊┊┊ FS BOUNDARY ┊┊┊
//!   ◄──────── { ok } ───────────────────────
//!   current_send_epoch[B] = epoch
//!   drop_old_epochs(keep_last_n)
//! ```
//!
//! ## Forward secrecy boundary
//!
//! Tras `take_ephemeral_sk` + decap, la esk del responder vive dentro
//! de un `Zeroizing<Vec<u8>>` que el handler dropea inmediatamente. A
//! partir de ese punto la esk NO está en memoria de ningún proceso, y
//! sólo el `shared_secret` permanece. Capturar la long-term sk en el
//! futuro **no permite descifrar este `master_secret`** porque la esk
//! que lo produjo ya no existe (es information-theoretically perdida).
//!
//! ## Backoff
//!
//! Errores transient (RPC failure, connect timeout, MAC inválido por
//! state desincronizado): backoff exponencial empezando en 250 ms y
//! cap a 30 s. Tras éxito el backoff se resetea.

use std::sync::Arc;
use std::time::Duration;

use common::crypto::pqc::kem_for;
use common::proto::common::v1::NodeId;
use common::proto::orr::v1::{
    orr_control_client::OrrControlClient, EstablishEphemeralSecretRequest,
    EstablishEphemeralSecretResponse, RequestEphemeralKeyRequest, RequestEphemeralKeyResponse,
};
use thiserror::Error;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use crate::macs::{
    mac_fin, mac_req, mac_resp, verify_mac_fin, verify_mac_req, verify_mac_resp, MacError,
};
use crate::peers::PeerRegistry;

/// Errores posibles de una rotación. Todos se tratan como transient
/// excepto `NoBootstrap` (que indica que el bootstrap inicial aún no
/// completó — sin clave HMAC no se puede rotar).
#[derive(Debug, Error)]
pub enum RotationError {
    #[error("no bootstrap_secret for peer {0} (initial bootstrap not complete)")]
    NoBootstrap(String),
    #[error("invalid gRPC URL: {0}")]
    BadUrl(String),
    #[error("transport: {0}")]
    Transport(#[from] tonic::transport::Error),
    #[error("rpc: {0}")]
    Rpc(#[from] tonic::Status),
    #[error("peer rejected: {0}")]
    Remote(String),
    #[error("mac verify failed")]
    Mac(#[from] MacError),
    #[error("pqc: {0}")]
    Pqc(#[from] common::crypto::pqc::PqcError),
    #[error("unexpected shared_secret length {0}")]
    BadSecretLen(usize),
}

/// Ejecuta UNA rotación completa contra `peer_id` (en `peer_addr`).
/// Devuelve el `epoch_id` recién acordado tras éxito. La función es
/// async pura: pega gRPC, hace 2 round-trips, y actualiza
/// `peers.master_secrets[peer]` + `current_send_epoch[peer]`. No
/// reintenta — eso lo hace el caller (`spawn_rotation_task`).
pub async fn run_one_rotation(
    suite: &str,
    local_orr_id: &str,
    peer_id: &str,
    peer_addr: &str,
    peers: &PeerRegistry,
) -> Result<u32, RotationError> {
    // 1. Leer bootstrap_secret (HMAC key). Sin él no se puede rotar.
    let bootstrap = peers
        .bootstrap_for(peer_id)
        .ok_or_else(|| RotationError::NoBootstrap(peer_id.to_string()))?;
    // 2. Elegir epoch monotónica: la siguiente al `current_send_epoch`.
    //    `unwrap_or(0).saturating_add(1)` → primera rotación = epoch 1.
    let epoch = peers
        .current_send_epoch(peer_id)
        .unwrap_or(0)
        .saturating_add(1);

    debug!(
        local = %local_orr_id,
        peer  = %peer_id,
        epoch,
        addr  = %peer_addr,
        "orr.rotation start",
    );

    // 3. mac_req + RPC RequestEphemeralKey.
    let mac1 = mac_req(&bootstrap, epoch, local_orr_id, peer_id);
    let channel = crate::grpc_tls::channel(peer_addr)
        .await
        .map_err(RotationError::BadUrl)?;
    let mut client = OrrControlClient::new(channel);

    let resp1 = client
        .request_ephemeral_key(RequestEphemeralKeyRequest {
            from: Some(NodeId {
                value: local_orr_id.to_string(),
            }),
            peer: Some(NodeId {
                value: peer_id.to_string(),
            }),
            epoch_id: epoch,
            mac: mac1,
        })
        .await?
        .into_inner();
    if !resp1.ok {
        return Err(RotationError::Remote(resp1.error));
    }

    // 4. Verificar MAC de la response y encap contra la epk efímera.
    verify_mac_resp(
        &bootstrap,
        epoch,
        local_orr_id,
        peer_id,
        &resp1.ephemeral_pubkey,
        &resp1.mac,
    )?;
    let kem = kem_for(suite)?;
    let encap = kem.encap(&resp1.ephemeral_pubkey)?;
    if encap.shared_secret.len() != 32 {
        return Err(RotationError::BadSecretLen(encap.shared_secret.len()));
    }
    let mut ss = [0u8; 32];
    ss.copy_from_slice(&encap.shared_secret);
    let ct = encap.ciphertext;

    // 5. mac_fin + RPC EstablishEphemeralSecret.
    let mac3 = mac_fin(&bootstrap, epoch, local_orr_id, peer_id, &ct);
    let resp2 = client
        .establish_ephemeral_secret(EstablishEphemeralSecretRequest {
            from: Some(NodeId {
                value: local_orr_id.to_string(),
            }),
            peer: Some(NodeId {
                value: peer_id.to_string(),
            }),
            epoch_id: epoch,
            ciphertext: ct,
            mac: mac3,
        })
        .await?
        .into_inner();
    if !resp2.ok {
        return Err(RotationError::Remote(resp2.error));
    }

    // 6. Commit local: nuevo master_secret + current_send_epoch.
    peers.set_master_for_epoch(peer_id.to_string(), epoch, ss);
    peers.set_current_send_epoch(peer_id.to_string(), epoch);
    info!(
        local = %local_orr_id,
        peer  = %peer_id,
        epoch,
        "orr.rotation committed",
    );
    Ok(epoch)
}

/// Spawnea una task tokio que rota con `peer_id` cada
/// `rotation_period_ms`. **Solo el lado lex-smaller** del par inicia
/// — el otro es reactivo (handlers gRPC). Si `local_orr_id >= peer_id`
/// retorna sin hacer nada.
///
/// Backoff exponencial cap 30 s en errores transient. Tras cada éxito
/// llama a `peers.drop_old_epochs(epoch_history_keep)` para liberar
/// secretos antiguos.
pub fn spawn_rotation_task(
    suite: String,
    local_orr_id: String,
    peer_id: String,
    peer_addr: String,
    peers: Arc<PeerRegistry>,
    rotation_period_ms: u64,
    epoch_history_keep: usize,
) {
    if local_orr_id >= peer_id {
        debug!(
            local = %local_orr_id,
            peer  = %peer_id,
            "orr.rotation passive side (peer initiates)"
        );
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(rotation_period_ms));
        // Por defecto `interval.tick().await` dispara inmediatamente al
        // primer await — consumimos esa primera salida para que el
        // primer rotation NO ocurra al instante (lo dispara el
        // bootstrap del OBJ-011 explícitamente).
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;
        let mut backoff = Duration::from_millis(250);
        let max_backoff = Duration::from_secs(30);
        loop {
            interval.tick().await;
            match run_one_rotation(&suite, &local_orr_id, &peer_id, &peer_addr, &peers).await {
                Ok(epoch) => {
                    info!(
                        local = %local_orr_id,
                        peer = %peer_id,
                        epoch,
                        "orr.rotation success",
                    );
                    peers.drop_old_epochs(epoch_history_keep);
                    // Resetear backoff tras éxito.
                    backoff = Duration::from_millis(250);
                }
                Err(e) => {
                    warn!(
                        local = %local_orr_id,
                        peer = %peer_id,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "orr.rotation failed; backing off",
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    });
}

// ─── Lado responder: handlers puros ────────────────────────────────────
//
// Los wrappers gRPC en `grpc_server.rs` delegan en estas dos funciones.
// Aislamos la lógica aquí para que (a) los tests in-process puedan
// usarlas sin levantar tonic y (b) el mock `TestRotationResponder`
// (ver módulo de tests más abajo) las reutilice byte-a-byte.

/// Lógica pura del handler `request_ephemeral_key` (sin tonic
/// wrapping). Devuelve el response listo para enviar. NUNCA `panic`
/// — todos los caminos de error van por `ok:false + error:String`.
pub fn handle_request_ephemeral_key(
    peers: &PeerRegistry,
    local_orr_id: &str,
    suite: &str,
    m: RequestEphemeralKeyRequest,
) -> RequestEphemeralKeyResponse {
    let from = m
        .from
        .as_ref()
        .map(|n| n.value.to_lowercase())
        .unwrap_or_default();
    let peer = m
        .peer
        .as_ref()
        .map(|n| n.value.to_lowercase())
        .unwrap_or_default();
    if from.is_empty() || peer.is_empty() {
        return RequestEphemeralKeyResponse {
            ok: false,
            error: "from/peer required".into(),
            ephemeral_pubkey: Vec::new(),
            mac: Vec::new(),
        };
    }
    if peer != local_orr_id {
        return RequestEphemeralKeyResponse {
            ok: false,
            error: format!("peer mismatch: expected {local_orr_id}, got {peer}"),
            ephemeral_pubkey: Vec::new(),
            mac: Vec::new(),
        };
    }
    let bootstrap = match peers.bootstrap_for(&from) {
        Some(b) => b,
        None => {
            return RequestEphemeralKeyResponse {
                ok: false,
                error: format!("no bootstrap_secret for {from} (initial bootstrap not complete)"),
                ephemeral_pubkey: Vec::new(),
                mac: Vec::new(),
            };
        }
    };
    if let Err(e) = verify_mac_req(&bootstrap, m.epoch_id, &from, &peer, &m.mac) {
        warn!(
            from = %from,
            peer = %peer,
            epoch = m.epoch_id,
            "orr.rotation mac_req invalid (drop)",
        );
        return RequestEphemeralKeyResponse {
            ok: false,
            error: format!("mac_req invalid: {e}"),
            ephemeral_pubkey: Vec::new(),
            mac: Vec::new(),
        };
    }
    let kem = match kem_for(suite) {
        Ok(k) => k,
        Err(e) => {
            return RequestEphemeralKeyResponse {
                ok: false,
                error: format!("kem suite {suite}: {e}"),
                ephemeral_pubkey: Vec::new(),
                mac: Vec::new(),
            };
        }
    };
    let kp = match kem.keygen() {
        Ok(k) => k,
        Err(e) => {
            return RequestEphemeralKeyResponse {
                ok: false,
                error: format!("keygen: {e}"),
                ephemeral_pubkey: Vec::new(),
                mac: Vec::new(),
            };
        }
    };
    peers.store_ephemeral_sk(from.clone(), m.epoch_id, kp.secret);
    let resp_mac = mac_resp(&bootstrap, m.epoch_id, &from, &peer, &kp.public);
    info!(
        from = %from,
        peer = %peer,
        epoch = m.epoch_id,
        pubkey_len = kp.public.len(),
        "orr.rotation epk issued",
    );
    RequestEphemeralKeyResponse {
        ok: true,
        error: String::new(),
        ephemeral_pubkey: kp.public,
        mac: resp_mac,
    }
}

/// Lógica pura del handler `establish_ephemeral_secret`.
pub fn handle_establish_ephemeral_secret(
    peers: &PeerRegistry,
    local_orr_id: &str,
    suite: &str,
    m: EstablishEphemeralSecretRequest,
) -> EstablishEphemeralSecretResponse {
    let from = m
        .from
        .as_ref()
        .map(|n| n.value.to_lowercase())
        .unwrap_or_default();
    let peer = m
        .peer
        .as_ref()
        .map(|n| n.value.to_lowercase())
        .unwrap_or_default();
    if from.is_empty() || peer.is_empty() {
        return EstablishEphemeralSecretResponse {
            ok: false,
            error: "from/peer required".into(),
        };
    }
    if peer != local_orr_id {
        return EstablishEphemeralSecretResponse {
            ok: false,
            error: format!("peer mismatch: expected {local_orr_id}, got {peer}"),
        };
    }
    let bootstrap = match peers.bootstrap_for(&from) {
        Some(b) => b,
        None => {
            return EstablishEphemeralSecretResponse {
                ok: false,
                error: format!("no bootstrap_secret for {from} (initial bootstrap not complete)"),
            };
        }
    };
    if let Err(e) = verify_mac_fin(&bootstrap, m.epoch_id, &from, &peer, &m.ciphertext, &m.mac) {
        warn!(
            from = %from,
            peer = %peer,
            epoch = m.epoch_id,
            "orr.rotation mac_fin invalid (drop)",
        );
        return EstablishEphemeralSecretResponse {
            ok: false,
            error: format!("mac_fin invalid: {e}"),
        };
    }
    let esk = match peers.take_ephemeral_sk(&from, m.epoch_id) {
        Some(e) => e,
        None => {
            return EstablishEphemeralSecretResponse {
                ok: false,
                error: format!(
                    "no ephemeral_sk for ({from}, epoch={}) — out-of-order FIN?",
                    m.epoch_id,
                ),
            };
        }
    };
    let kem = match kem_for(suite) {
        Ok(k) => k,
        Err(e) => {
            return EstablishEphemeralSecretResponse {
                ok: false,
                error: format!("kem suite {suite}: {e}"),
            };
        }
    };
    let ss_bytes = match kem.decap(&esk[..], &m.ciphertext) {
        Ok(s) => s,
        Err(e) => {
            // FS boundary: la esk se dropea aquí (zeroizing wipes)
            // aun cuando el decap falla.
            return EstablishEphemeralSecretResponse {
                ok: false,
                error: format!("decap: {e}"),
            };
        }
    };
    // Drop explícito: documenta la FS boundary inmediata tras decap.
    drop(esk);

    if ss_bytes.len() != 32 {
        return EstablishEphemeralSecretResponse {
            ok: false,
            error: format!("shared_secret unexpected len {}", ss_bytes.len()),
        };
    }
    let mut ss = [0u8; 32];
    ss.copy_from_slice(&ss_bytes);
    peers.set_master_for_epoch(from.clone(), m.epoch_id, ss);
    info!(
        from = %from,
        peer = %peer,
        epoch = m.epoch_id,
        "orr.rotation master_secret installed (responder side)",
    );
    EstablishEphemeralSecretResponse {
        ok: true,
        error: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Sin bootstrap previo, `run_one_rotation` falla limpio con
    /// `NoBootstrap` antes de tocar la red. Validamos la guardia
    /// inicial sin necesidad de servidor gRPC.
    #[tokio::test]
    async fn run_one_rotation_errors_without_bootstrap() {
        let peers = PeerRegistry::new(HashMap::new(), "orr_a".into(), 1);
        // Notar: NO llamamos `set_bootstrap` — el registry está vacío
        // para `orr_b`.
        let err = run_one_rotation(
            "ml-kem-768",
            "orr_a",
            "orr_b",
            "http://127.0.0.1:1", // irrelevante: nunca llega a la red
            &peers,
        )
        .await
        .expect_err("expected NoBootstrap");
        assert!(matches!(err, RotationError::NoBootstrap(_)));
    }

    /// `spawn_rotation_task` con local >= peer lex retorna sin
    /// spawnear nada (lado pasivo). No podemos verificar directamente
    /// que no spawnea, pero el método retorna inmediato y no panic.
    #[tokio::test]
    async fn spawn_rotation_task_passive_side_is_noop() {
        let peers = Arc::new(PeerRegistry::new(HashMap::new(), "orr_z".into(), 1));
        // local "orr_z" > peer "orr_a" lex → pasivo.
        spawn_rotation_task(
            "ml-kem-768".into(),
            "orr_z".into(),
            "orr_a".into(),
            "http://127.0.0.1:1".into(),
            peers,
            30_000,
            3,
        );
        // Sin assertion: si llegamos aquí sin panic es OK. La task no
        // debería existir; un yield para confirmar runtime healthy.
        tokio::task::yield_now().await;
    }

    // ─── OBJ-014: tests end-to-end con responder gRPC efímero ─────────
    //
    // Montamos un mini-servidor tonic que sólo implementa los 2 RPCs
    // de rotación (resto = `unimplemented`). El cliente
    // `run_one_rotation` hace 2 round-trips reales contra ese
    // servidor sobre un puerto random de loopback. Esto valida la
    // capa cripto + MAC + estado de extremo a extremo, sin necesidad
    // de levantar `OrrService` completo (que requiere QKC TCP +
    // SDN gRPC + pump async).

    use common::proto::common::v1::Status as ProtoStatus;
    use common::proto::orr::v1::{
        orr_control_server::{OrrControl, OrrControlServer},
        Circuit as ProtoCircuit, CloseCircuitRequest, DeliveredMessage, EstablishSecretRequest,
        EstablishSecretResponse, GetCircuitRequest, GetPublicKeyRequest, GetPublicKeyResponse,
        ListCircuitsRequest, OpenCircuitRequest, OpenCircuitResponse, RelayFrame,
        SendMessageRequest, SendMessageResponse, StreamDeliveriesRequest,
    };
    use tokio::sync::oneshot;
    use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
    use tonic::{transport::Server, Request, Response, Status};

    /// Mock del `OrrControl` que sólo implementa las 2 RPCs de
    /// rotación. El resto devuelve `unimplemented`. Los handlers
    /// reales se delegan a `handle_request_ephemeral_key` /
    /// `handle_establish_ephemeral_secret` para que la lógica sea
    /// idéntica a producción.
    struct TestRotationResponder {
        orr_id: String,
        suite: String,
        peers: Arc<PeerRegistry>,
    }

    #[tonic::async_trait]
    impl OrrControl for TestRotationResponder {
        type ListCircuitsStream = ReceiverStream<std::result::Result<ProtoCircuit, Status>>;
        type StreamDeliveriesStream = ReceiverStream<std::result::Result<DeliveredMessage, Status>>;

        async fn send_message(
            &self,
            _req: Request<SendMessageRequest>,
        ) -> std::result::Result<Response<SendMessageResponse>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn stream_deliveries(
            &self,
            _req: Request<StreamDeliveriesRequest>,
        ) -> std::result::Result<Response<Self::StreamDeliveriesStream>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn get_public_key(
            &self,
            _req: Request<GetPublicKeyRequest>,
        ) -> std::result::Result<Response<GetPublicKeyResponse>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn establish_secret(
            &self,
            _req: Request<EstablishSecretRequest>,
        ) -> std::result::Result<Response<EstablishSecretResponse>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn request_ephemeral_key(
            &self,
            req: Request<RequestEphemeralKeyRequest>,
        ) -> std::result::Result<Response<RequestEphemeralKeyResponse>, Status> {
            let resp = handle_request_ephemeral_key(
                &self.peers,
                &self.orr_id,
                &self.suite,
                req.into_inner(),
            );
            Ok(Response::new(resp))
        }

        async fn establish_ephemeral_secret(
            &self,
            req: Request<EstablishEphemeralSecretRequest>,
        ) -> std::result::Result<Response<EstablishEphemeralSecretResponse>, Status> {
            let resp = handle_establish_ephemeral_secret(
                &self.peers,
                &self.orr_id,
                &self.suite,
                req.into_inner(),
            );
            Ok(Response::new(resp))
        }

        async fn open_circuit(
            &self,
            _req: Request<OpenCircuitRequest>,
        ) -> std::result::Result<Response<OpenCircuitResponse>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn close_circuit(
            &self,
            _req: Request<CloseCircuitRequest>,
        ) -> std::result::Result<Response<ProtoStatus>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn get_circuit(
            &self,
            _req: Request<GetCircuitRequest>,
        ) -> std::result::Result<Response<ProtoCircuit>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn list_circuits(
            &self,
            _req: Request<ListCircuitsRequest>,
        ) -> std::result::Result<Response<Self::ListCircuitsStream>, Status> {
            Err(Status::unimplemented("test stub"))
        }

        async fn relay(
            &self,
            _req: Request<RelayFrame>,
        ) -> std::result::Result<Response<ProtoStatus>, Status> {
            Err(Status::unimplemented("test stub"))
        }
    }

    /// Helper que spawnea un responder en un puerto random de
    /// loopback. Devuelve `(url, peers, shutdown_tx)`. El caller
    /// llama `shutdown_tx.send(())` al final del test para que el
    /// servidor termine limpio (evita resource leaks en el test
    /// runner).
    async fn spawn_test_responder(
        orr_id: &str,
        suite: &str,
    ) -> (String, Arc<PeerRegistry>, oneshot::Sender<()>) {
        let peers = Arc::new(PeerRegistry::new(
            std::collections::HashMap::new(),
            orr_id.into(),
            0,
        ));
        let svc = TestRotationResponder {
            orr_id: orr_id.into(),
            suite: suite.into(),
            peers: peers.clone(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("http://{addr}");
        let incoming = TcpListenerStream::new(listener);
        let (tx, rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(OrrControlServer::new(svc))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = rx.await;
                })
                .await;
        });
        // Pequeño yield para que el server entre al accept-loop antes
        // de que el cliente intente conectar.
        tokio::task::yield_now().await;
        (url, peers, tx)
    }

    /// Test 1: 2 ORRs locales + bootstrap sembrado + 2 rotaciones
    /// completas. Ambos lados terminan con `master_secrets[*][1]` y
    /// `[*][2]` idénticos, y el responder tiene `ephemeral_sks`
    /// vacío al final.
    #[tokio::test]
    async fn rotation_two_orrs_local_endpoint() {
        let (url_b, peers_b, shutdown_b) = spawn_test_responder("orr_b", "ml-kem-768").await;
        // El initiator (peers_a) y el responder (peers_b) comparten
        // el mismo bootstrap_secret (resultado simétrico del encap
        // inicial). En producción lo deriva `bootstrap.rs`; aquí lo
        // sembramos directo para aislar el test a la rotación.
        let bootstrap = [0xAB; 32];
        let peers_a = PeerRegistry::new(std::collections::HashMap::new(), "orr_a".into(), 0);
        peers_a.set_bootstrap("orr_b".into(), bootstrap);
        peers_b.set_bootstrap("orr_a".into(), bootstrap);

        // Rotación 1 (epoch 1).
        let epoch_1 = run_one_rotation("ml-kem-768", "orr_a", "orr_b", &url_b, &peers_a)
            .await
            .expect("rotation 1");
        assert_eq!(epoch_1, 1);
        let ms_a_1 = peers_a
            .master_for_epoch("orr_b", 1)
            .expect("peers_a master 1");
        let ms_b_1 = peers_b
            .master_for_epoch("orr_a", 1)
            .expect("peers_b master 1");
        assert_eq!(ms_a_1, ms_b_1, "epoch 1 master_secret must match");

        // Rotación 2 (epoch 2).
        let epoch_2 = run_one_rotation("ml-kem-768", "orr_a", "orr_b", &url_b, &peers_a)
            .await
            .expect("rotation 2");
        assert_eq!(epoch_2, 2);
        let ms_a_2 = peers_a
            .master_for_epoch("orr_b", 2)
            .expect("peers_a master 2");
        let ms_b_2 = peers_b
            .master_for_epoch("orr_a", 2)
            .expect("peers_b master 2");
        assert_eq!(ms_a_2, ms_b_2, "epoch 2 master_secret must match");
        // Y son distintos entre épocas (keypair efímera fresca).
        assert_ne!(ms_a_1, ms_a_2, "epoch 1 and 2 secrets must differ");

        // El responder debe tener `ephemeral_sks` vacío al final: las
        // 2 esks generadas se consumieron en sus respectivos FIN.
        assert!(
            !peers_b.has_ephemeral_sk("orr_a", 1),
            "esk for epoch 1 should be zeroized after FIN"
        );
        assert!(
            !peers_b.has_ephemeral_sk("orr_a", 2),
            "esk for epoch 2 should be zeroized after FIN"
        );

        let _ = shutdown_b.send(());
    }

    /// Test 2: `mac_req` corrupto en el REQ → server responde
    /// `ok:false`, NO se mutan estados. El cliente lo reporta como
    /// `RotationError::Remote("mac_req invalid: ...")`.
    #[tokio::test]
    async fn rotation_rejects_bad_mac() {
        let (url_b, peers_b, shutdown_b) = spawn_test_responder("orr_b", "ml-kem-768").await;
        let peers_a = PeerRegistry::new(std::collections::HashMap::new(), "orr_a".into(), 0);
        // `peers_a` tiene un bootstrap CORRECTO pero `peers_b` tiene
        // OTRO bootstrap: el server no puede validar el MAC del
        // cliente porque las HMAC-keys difieren.
        peers_a.set_bootstrap("orr_b".into(), [0xAB; 32]);
        peers_b.set_bootstrap("orr_a".into(), [0xCD; 32]);

        let err = run_one_rotation("ml-kem-768", "orr_a", "orr_b", &url_b, &peers_a)
            .await
            .expect_err("expected Remote(mac_req invalid)");
        match err {
            RotationError::Remote(msg) => {
                assert!(
                    msg.contains("mac_req invalid"),
                    "expected mac_req invalid, got: {msg}",
                );
            }
            other => panic!("expected Remote, got {other:?}"),
        }
        // Ningún master_secret se ha guardado en ningún lado.
        assert!(peers_a.master_for_epoch("orr_b", 1).is_none());
        assert!(peers_b.master_for_epoch("orr_a", 1).is_none());
        // Tampoco se ha avanzado `current_send_epoch` en el initiator
        // (sólo se incrementa tras éxito).
        assert!(peers_a.current_send_epoch("orr_b").is_none());
        // El responder tampoco tiene esk almacenada (la guardia MAC
        // sale ANTES del keygen).
        assert!(!peers_b.has_ephemeral_sk("orr_a", 1));

        let _ = shutdown_b.send(());
    }

    /// Test 3: 5 rotaciones consecutivas con `keep_last_n = 3`.
    /// Tras `drop_old_epochs(3)` en cada lado, sólo las épocas
    /// 3..=5 sobreviven en `master_secrets`.
    #[tokio::test]
    async fn rotation_drops_old_epochs_beyond_keep() {
        let (url_b, peers_b, shutdown_b) = spawn_test_responder("orr_b", "ml-kem-768").await;
        let peers_a = PeerRegistry::new(std::collections::HashMap::new(), "orr_a".into(), 0);
        let bootstrap = [0xEE; 32];
        peers_a.set_bootstrap("orr_b".into(), bootstrap);
        peers_b.set_bootstrap("orr_a".into(), bootstrap);

        // 5 rotaciones (epoch 1..=5).
        for expected in 1..=5u32 {
            let got = run_one_rotation("ml-kem-768", "orr_a", "orr_b", &url_b, &peers_a)
                .await
                .unwrap_or_else(|e| panic!("rotation {expected} failed: {e}"));
            assert_eq!(got, expected);
            peers_a.drop_old_epochs(3);
            peers_b.drop_old_epochs(3);
        }

        // 1..=2 deben haberse zeroizado en ambos lados.
        for e in 1..=2u32 {
            assert!(
                peers_a.master_for_epoch("orr_b", e).is_none(),
                "peers_a: epoch {e} should be dropped"
            );
            assert!(
                peers_b.master_for_epoch("orr_a", e).is_none(),
                "peers_b: epoch {e} should be dropped"
            );
        }
        // 3..=5 deben seguir presentes en ambos lados Y coincidir.
        for e in 3..=5u32 {
            let ms_a = peers_a
                .master_for_epoch("orr_b", e)
                .unwrap_or_else(|| panic!("peers_a: epoch {e} missing"));
            let ms_b = peers_b
                .master_for_epoch("orr_a", e)
                .unwrap_or_else(|| panic!("peers_b: epoch {e} missing"));
            assert_eq!(ms_a, ms_b, "epoch {e} must match");
        }
        // `latest_epoch_for` ahora apunta a 5 en ambos lados.
        assert_eq!(peers_a.latest_epoch_for("orr_b"), Some(5));
        assert_eq!(peers_b.latest_epoch_for("orr_a"), Some(5));

        let _ = shutdown_b.send(());
    }
}
