//! Bootstrap de master_secrets entre pares de ORRs.
//!
//! Al arrancar cada ORR conoce sus peers vía `peer_grpc_addrs` (TOML).
//! Para cada peer:
//!
//!   1. Pide la pubkey vía `GetPublicKey` (con backoff hasta éxito).
//!   2. Hace `kem.encap(peer.pubkey) → (ct, ss)`. Guarda `ss` como
//!      `master_secrets[peer]`.
//!   3. Llama a `peer.EstablishSecret(from=self, ciphertext=ct)`. El
//!      peer decapsula con su sk y guarda `master_secrets[self] = ss`.
//!
//! Tras esto, cada par `(A,P)` comparte un `master_secret` de 32 B (en
//! realidad dos, uno por dirección — A inicia con `kem.encap(P.pk)` y P
//! inicia con `kem.encap(A.pk)`; los usamos en la dirección que toca).
//!
//! El cliente (`A`) usa `master_secrets[P]` (= shared_secret obtenido
//! por A al hacer encap → P decap) para cifrar frames hacia P. El
//! servidor (`P`) usa `master_secrets[A]` (= shared_secret obtenido por
//! A al hacer encap, recibido por P vía decap) para descifrar frames
//! que vengan de A.
//!
//! Es decir: `master_secrets[X]` siempre significa "el secret que
//! comparto con X cuando X es el origen del frame". Coincide con el
//! flujo: el initiator del encap es el origen del frame, y todos los
//! ORRs intermedios del onion-path usan el secret que comparten con el
//! initiator.

use std::sync::Arc;
use std::time::Duration;

use common::proto::common::v1::NodeId;
use common::proto::orr::v1::{
    orr_control_client::OrrControlClient, EstablishSecretRequest, GetPublicKeyRequest,
};
use tracing::{debug, info, warn};

use crate::{handshake, identity::OrrIdentity, peers::PeerRegistry};

/// Estado del bootstrap por peer. Útil para `/healthz`.
#[derive(Debug, Clone, Copy)]
pub enum BootstrapState {
    PubkeyMissing,
    SecretMissing,
    Done,
}

/// Lanza una task background por peer. Cada una hace pubkey-fetch +
/// secret-establish con backoff y persiste hasta éxito (o muerte del
/// proceso).
pub fn spawn_all(
    identity: Arc<OrrIdentity>,
    peers: Arc<PeerRegistry>,
    peer_grpc_addrs: std::collections::HashMap<String, String>,
    suite: String,
    rotation_period_ms: u64,
    epoch_history_keep: usize,
) {
    for (peer_id, addr) in peer_grpc_addrs {
        spawn_one(
            identity.clone(),
            peers.clone(),
            peer_id,
            addr,
            suite.clone(),
            rotation_period_ms,
            epoch_history_keep,
        );
    }
}

/// Lanza el bootstrap de **un** peer. Extraído de [`spawn_all`] para poder
/// darlos de alta en caliente: cuando la SDN le dice a este ORR que hay un peer
/// nuevo, se llama aquí y la task hace su pubkey-fetch + secret-establish con
/// backoff, igual que si hubiera estado en el `node.yml` desde el principio.
///
/// El llamante es responsable de no invocarlo dos veces para el mismo peer: la
/// task persiste hasta lograrlo, así que dos serían dos bootstraps compitiendo.
#[allow(clippy::too_many_arguments)]
pub fn spawn_one(
    identity: Arc<OrrIdentity>,
    peers: Arc<PeerRegistry>,
    peer_id: String,
    addr: String,
    suite: String,
    rotation_period_ms: u64,
    epoch_history_keep: usize,
) {
    let local_orr_id = peers.local_orr_id().to_string();
    // Único punto por el que pasan tanto los peers del `node.yml` como los
    // que manda la SDN, así que es donde se registra su URL. El
    // re-bootstrap pasivo la necesita para poder rehacer el handshake si
    // luego perdemos el `master_secret` con este peer.
    peers.put_grpc_addr(peer_id.clone(), addr.clone());
    tokio::spawn(async move {
        bootstrap_peer(
            identity,
            peers,
            local_orr_id,
            peer_id,
            addr,
            suite,
            rotation_period_ms,
            epoch_history_keep,
        )
        .await;
    });
}

async fn bootstrap_peer(
    identity: Arc<OrrIdentity>,
    peers: Arc<PeerRegistry>,
    local_orr_id: String,
    peer_id: String,
    addr: String,
    suite: String,
    rotation_period_ms: u64,
    epoch_history_keep: usize,
) {
    // Fase 1: asegurar pubkey en el registry (necesaria para encap, y
    // útil incluso si vamos a ser el lado pasivo del handshake — la
    // pubkey es info pública y nos sirve para diagnóstico).
    if peers.public_key(&peer_id).is_none() {
        fetch_pubkey(&peers, &local_orr_id, &peer_id, &addr).await;
    }

    // Fase 2: handshake con initiator determinista para evitar la
    // race condition de bootstraps concurrentes.
    //
    // Si los dos ORRs de un par hacen `kem.encap` a la vez, cada uno
    // produce un shared_secret distinto. Tras los dos `EstablishSecret`
    // RPC cruzados, cada lado se queda con un secret diferente (el
    // último put gana), y al cifrar un frame el peeler no puede
    // descifrar — el plaintext sale ruido y `InnerLayer::decode` falla
    // (que es exactamente el síntoma que vimos en mode 2 con 99/120).
    //
    // Solución: solo el ORR con `orr_id` lexicográficamente menor
    // inicia el encap. El otro espera a recibir el RPC y guarda en su
    // handler de `establish_secret`. Garantiza un único initiator por
    // par → un único shared_secret → ambos lados coinciden.
    //
    // Tras OBJ-011 (audit H-3 / Option B), ese shared_secret se guarda
    // como `bootstrap_secret` (NO como master_secret) y se usa solo
    // como clave HMAC para autenticar las RPCs de rotación. El
    // master_secret real por época nace de cada rotación con keypair
    // ML-KEM efímera fresca (ver `rotation.rs`).
    if local_orr_id >= peer_id {
        debug!(
            peer = %peer_id,
            "orr.bootstrap passive side (peer initiates)"
        );
        return;
    }

    // Idempotente: si ya hubo otra task que nos rellenó el bootstrap,
    // saltamos el encap pero igual disparamos rotación + spawn por si
    // este peer reinició y perdió `master_secrets`.
    if !peers.has_bootstrap(&peer_id) {
        let pk = match peers.public_key(&peer_id) {
            Some(p) => p,
            None => {
                warn!(peer = %peer_id, "orr.bootstrap pubkey still missing — aborting bootstrap");
                return;
            }
        };

        let mut backoff = Duration::from_millis(250);
        let max_backoff = Duration::from_secs(30);
        loop {
            // `has_bootstrap` es un check-then-act y no basta para excluir al
            // otro camino que también hace encap: `trigger_passive_rebootstrap`.
            // Si los dos corren a la vez, cada uno genera un shared_secret
            // distinto, el peer se queda con el último que le llega y este
            // lado con el suyo — y a partir de ahí todo lo que se cifre entre
            // ellos sale ruido. Observado el 2026-08-02 al dar de alta un nodo
            // en caliente: orr_4 guardó DOS bootstrap_secret de orr_1 con 28 ms
            // de diferencia y el par 1↔4 quedó entregando el 100 % de las
            // claves de transporte corruptas (lo cazó el `key_digest` del DKMS,
            // que es lo único que hay ahí abajo mirando).
            //
            // El flag se toma por intento, no durante todo el bucle: así el
            // camino pasivo —que además refresca la pubkey— puede intervenir
            // entre reintentos si este handshake no consigue converger.
            if peers.has_bootstrap(&peer_id) {
                break;
            }
            if !peers.try_mark_rebootstrap_inflight(&peer_id) {
                debug!(
                    peer = %peer_id,
                    "orr.bootstrap espera: otro establecimiento en vuelo para este par"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
            let attempt =
                attempt_establish(&identity, &suite, &pk, &local_orr_id, &peer_id, &addr).await;
            peers.clear_rebootstrap_inflight(&peer_id);
            match attempt {
                Ok(secret) => {
                    // OBJ-011: guardar como bootstrap_secret (HMAC key),
                    // NO como master_secret.
                    //
                    // Workaround: cableamos también como master_secret
                    // de epoch 0 en este lado. Sin esto la rotación
                    // Option-B se desincroniza entre initiator/responder
                    // y los frames se dropean con "missing epoch".
                    // Ver comentario gemelo en `grpc_server.rs`.
                    peers.set_bootstrap(peer_id.clone(), secret);
                    peers.set_master_for_epoch(peer_id.clone(), 0, secret);
                    info!(
                        local = %local_orr_id,
                        peer  = %peer_id,
                        addr  = %addr,
                        "orr.bootstrap bootstrap_secret ok"
                    );
                    break;
                }
                Err(e) => {
                    debug!(
                        peer = %peer_id,
                        addr = %addr,
                        error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "orr.bootstrap establish_secret retry",
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                }
            }
        }
    } else {
        debug!(peer = %peer_id, "orr.bootstrap bootstrap_secret already present");
    }

    // Phase 3 + Phase 4 (rotación OBJ-011) deshabilitadas. La rotación
    // actual (run_one_rotation + spawn_rotation_task) sólo actualiza el
    // lado initiator (lex-smaller) y deja al passive con epoch=0,
    // causando frames con `epoch=0` que el receiver dropea con
    // `latest=Some(1)`. Hasta que el sub-protocolo propague la nueva
    // época al passive side antes de que el initiator suba el contador
    // local, mantenemos epoch=0 estable en ambos lados (master_secret
    // sembrado desde el bootstrap_secret arriba).
    let _ = rotation_period_ms;
    let _ = epoch_history_keep;
    let _ = suite;
    let _ = local_orr_id;
    let _ = peer_id;
    let _ = addr;
    let _ = peers;
}

async fn fetch_pubkey(peers: &PeerRegistry, local_orr_id: &str, peer_id: &str, addr: &str) {
    let mut backoff = Duration::from_millis(250);
    let max_backoff = Duration::from_secs(30);
    loop {
        match try_fetch_pubkey(addr).await {
            Ok((pk, suite, reported_id, signature)) => {
                let reported_lc = reported_id.to_lowercase();
                if !reported_lc.is_empty() && reported_lc != peer_id {
                    warn!(
                        expected = %peer_id,
                        reported = %reported_id,
                        addr = %addr,
                        "peer pubkey reports a different orr_id; ignoring",
                    );
                    return;
                }
                // §Fase 6 PQC: verifica la firma ML-DSA del anuncio.
                //
                // Una firma VÁLIDA autentica el anuncio por sí sola y hace
                // innecesario el pin: el pin existía para cuando no había
                // firma. Exigir ambos es incoherente — la pubkey ML-KEM del ORR
                // es efímera (se regenera en cada arranque), así que nunca hay
                // pin que case y `strict` rechazaba TODO aunque la firma fuese
                // correcta (medido: 12 rechazos en una malla de 4 nodos).
                let sig = peers.verify_announcement(peer_id, &suite, &pk, &signature);
                let firmado = matches!(sig, crate::peers::SigVerdict::Valid);
                match sig {
                    crate::peers::SigVerdict::Reject => {
                        warn!(
                            local = %local_orr_id, peer = %peer_id, addr = %addr,
                            "orr.peer_pubkey: firma ML-DSA del anuncio inválida/ausente; rechazo",
                        );
                        return;
                    }
                    crate::peers::SigVerdict::Unsigned => warn!(
                        peer = %peer_id,
                        "orr.peer_pubkey: el peer no firmó su anuncio (sin sign_secret_seed); tofu lo acepta",
                    ),
                    crate::peers::SigVerdict::NoKey => debug!(
                        peer = %peer_id,
                        "orr.peer_pubkey: sin peer_verify_key configurada; no verifico firma (tofu)",
                    ),
                    crate::peers::SigVerdict::Valid => debug!(
                        peer = %peer_id, "orr.peer_pubkey: firma ML-DSA válida",
                    ),
                }
                // §Fase 6: contrasta la pubkey anunciada contra el pin de
                // `peer_pubkeys` según `bootstrap_trust`.
                // El pin solo decide si el anuncio NO venía firmado.
                match if firmado {
                    crate::peers::PubkeyVerdict::Accept
                } else {
                    peers.verify_fetched_pubkey(peer_id, &pk)
                } {
                    crate::peers::PubkeyVerdict::Reject => {
                        warn!(
                            local = %local_orr_id, peer = %peer_id, addr = %addr,
                            "orr.peer_pubkey: strict + sin pin que case; rechazo la pubkey anunciada",
                        );
                        return;
                    }
                    crate::peers::PubkeyVerdict::AcceptPinMismatch => {
                        warn!(
                            local = %local_orr_id, peer = %peer_id, addr = %addr,
                            "orr.peer_pubkey: la pubkey anunciada DIFIERE del pin configurado \
                             (reinicio del peer con identidad efímera, o MITM); la acepto (tofu)",
                        );
                    }
                    crate::peers::PubkeyVerdict::Accept => {}
                }
                info!(
                    local = %local_orr_id,
                    peer  = %peer_id,
                    addr  = %addr,
                    suite = %suite,
                    pubkey_len = pk.len(),
                    "orr.peer_pubkey bootstrap ok",
                );
                peers.put_pubkey(peer_id.to_string(), pk);
                return;
            }
            Err(e) => {
                debug!(
                    peer = %peer_id,
                    addr = %addr,
                    error = %e,
                    backoff_ms = backoff.as_millis() as u64,
                    "orr.peer_pubkey bootstrap retry",
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Pide la pubkey del peer vía `GetPublicKey` RPC. Re-fetcheable: el
/// caller (`bootstrap_peer` initial o `trigger_passive_rebootstrap`)
/// la usa para invalidar la pubkey cacheada cuando hay sospecha de
/// que el peer reinició (e.g. tras un `decap_failed` en
/// `EstablishSecret`).
///
/// `pub(crate)` para que `OrrService::trigger_passive_rebootstrap`
/// pueda refrescar la pubkey antes de cada intento de re-handshake
/// — sin esto, un peer que reinició produce ciphertexts que su nueva
/// sk no puede decapsular y el rebootstrap se queda en bucle infinito
/// (verificado smoke 2026-05-25 n10-real16k).
pub(crate) async fn try_fetch_pubkey(
    addr: &str,
) -> std::result::Result<(Vec<u8>, String, String, Vec<u8>), String> {
    let ch = crate::grpc_tls::channel(addr).await?;
    let mut client = OrrControlClient::new(ch);
    let resp = client
        .get_public_key(GetPublicKeyRequest {})
        .await
        .map_err(|s| format!("rpc: {s}"))?
        .into_inner();
    let id = resp.orr_id.map(|n| n.value).unwrap_or_default();
    Ok((resp.public_key, resp.suite, id, resp.signature))
}

/// Hace el encap contra `pk`, llama a `EstablishSecret` y devuelve el
/// shared_secret de 32 B si la RPC confirma ok.
///
/// `pub` desde OBJ "passive re-bootstrap": el handler de
/// `OrrService::trigger_passive_rebootstrap` reutiliza esta función
/// para reactivar un peer cuyo `master_secret` se desincronizó tras un
/// restart del pod.
pub async fn attempt_establish(
    _identity: &OrrIdentity,
    suite: &str,
    pk: &[u8],
    local_orr_id: &str,
    peer_id: &str,
    addr: &str,
) -> std::result::Result<[u8; 32], String> {
    let encap = handshake::initiate(suite, pk).map_err(|e| format!("encap: {e}"))?;
    let mut secret = [0u8; 32];
    if encap.shared_secret.len() != 32 {
        return Err(format!(
            "shared_secret unexpected len {} (expected 32)",
            encap.shared_secret.len()
        ));
    }
    secret.copy_from_slice(&encap.shared_secret);

    let ch = crate::grpc_tls::channel(addr).await?;
    let mut client = OrrControlClient::new(ch);
    let resp = client
        .establish_secret(EstablishSecretRequest {
            from: Some(NodeId {
                value: local_orr_id.to_string(),
            }),
            ciphertext: encap.ciphertext,
        })
        .await
        .map_err(|s| format!("rpc: {s}"))?
        .into_inner();
    if !resp.ok {
        return Err(format!("peer {peer_id} rejected: {}", resp.error));
    }
    Ok(secret)
}
