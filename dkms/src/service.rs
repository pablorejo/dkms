//! Núcleo de orquestación del DKMS.
//!
//! Aquí viven los métodos que invocan los handlers HTTP (norte y este/oeste)
//! y la lógica que cose el estado seguro en memoria con la distribución de
//! claves vía ETSI 020. Nada que hable directamente con el cable: los
//! sockets están en [`crate::etsi_http`] y los clientes gRPC en
//! [`crate::southbound`].
//!
//! ### Flujo `enc_keys` (resumen)
//!
//! 1. Autenticación mTLS por el handler → llegamos con un `master_sae`
//!    confiable.
//! 2. Resolución de destinos: `slave + additional_*` → cada uno a su
//!    DKMS (vía `sae_binding`).  Agrupamos por DKMS.
//! 3. Coste = `⌈size_bytes / token_unit⌉ · number · |dkms_destinos|`,
//!    se consume del bucket del `master`.
//! 4. Por cada `K` (`number` veces) generamos bytes aleatorios + `key_id`.
//!    Los SAEs locales autorizados quedan registrados en el `PendingStore`.
//! 5. Para cada DKMS remoto se envía **una** ETSI 020 (`/kmapi/v1/ext_keys`)
//!    con sus `target_sae_ids`, envolviendo cada `K` con **una** clave de
//!    transporte fresca de `buffer_enc[peer]` (AEAD `ChaCha20Poly1305`).
//! 6. Política de fallo "a": si cualquier destino DKMS no ACK-ea →
//!    reembolso de tokens, retracción de las entradas locales y 502.
//! 7. Si todo OK, se responde al master con un `Etsi014KeyContainer`.
//!
//! ### Flujo `dec_keys`
//!
//! 1. `take_for_sae` en el `PendingStore` con la identidad mTLS del SAE.
//! 2. Si la entrada existe y el SAE está autorizado, se devuelve `K` y se
//!    marca como consumida (one-shot por SAE). Cuando todos los autorizados
//!    la han recogido, la entrada se borra y los bytes se zeroizan.
//!
//! ### Flujo entrante ETSI 020
//!
//! 1. mTLS valida que el peer es un DKMS vecino; el handler nos da su
//!    `NodeId`.
//! 2. Por cada `key.value`, leemos `transport_key_id` en `extension`,
//!    sacamos esa clave de `buffer_dec[peer]` y desenvolvemos `K`.
//! 3. Insertamos `K` en el `PendingStore` local con
//!    `authorized = target_sae_ids` y el TTL del container.

use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
    time::Duration,
};

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305, Nonce,
};
use futures::future::join_all;
use rand::{rngs::OsRng, RngCore};
use serde_json::Value;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

use common::{
    ids::{KeyId, NodeId, SaeId},
    metrics::Metrics,
};
use etsi::{
    base64bytes::Base64Bytes,
    v014::{Etsi014Key, Etsi014KeyContainer, Etsi014KeyIDs, Etsi014KeyRequest, Etsi014Status},
    v020::{
        ack_status::Etsi020AckStatus, Etsi020ExtKeyAckContainer, Etsi020ExtKeyContainer,
        Etsi020Key, Etsi020KeyID,
    },
};

use crate::{
    admission::Admission,
    config::{DkmsConfig, PeerTransport},
    error::{DkmsError, Result},
    peer_client::PeerHttpClient,
    sae_binding::SaeBindingCache,
    southbound::{
        orr::{
            HDR_FLOW_ID, HDR_KEY_ID, HDR_KEY_SIZE_BITS, HDR_REQUEST_ID, HDR_SAE_DESTINATION,
            HDR_SAE_ORIGIN, HDR_TIMESTAMP_MS,
        },
        OrrClient, QkcClient, SdnClient,
    },
    state::{BufferPool, PendingStore},
    token_bucket::{compute_cost, SaeBuckets},
};

/// Tamaño en bytes de la clave de transporte AEAD (ChaCha20-Poly1305).
const TRANSPORT_KEY_BYTES: usize = 32;
/// Tamaño del nonce ChaCha20-Poly1305.
const NONCE_BYTES: usize = 12;

#[derive(Clone)]
pub struct DkmsService {
    pub cfg: Arc<DkmsConfig>,
    pub metrics: Metrics,

    pub pool: Arc<BufferPool>,
    pub pending: Arc<PendingStore>,
    pub buckets: Arc<SaeBuckets>,
    pub sae_binding: Arc<SaeBindingCache>,

    pub sdn: Option<Arc<SdnClient>>,
    pub qkc: Option<Arc<QkcClient>>,
    /// Cliente gRPC al ORR co-localizado. Si está presente, el DKMS
    /// puede usar el transporte ORR↔QKC (binario sobre TCP) como
    /// alternativa al HTTP/2 ETSI 020 entre DKMSs. Se cablea como
    /// `Option` para no romper despliegues sin ORR.
    pub orr: Option<Arc<OrrClient>>,
    /// Cliente HTTP/2 ETSI 020 hacia DKMSs peer. `None` en despliegues
    /// donde todos los peers usan `transport = "orr"` (no se intenta
    /// abrir un Reqwest TLS context si nadie lo usa).
    pub peer_client: Option<Arc<PeerHttpClient>>,

    /// Estado de admisión — consultado por el `AdmissionLayer` del HTTP y
    /// pilotado por el RPC `Drain` del plano gRPC.
    pub admission: Arc<Admission>,
}

impl DkmsService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<DkmsConfig>,
        metrics: Metrics,
        pool: Arc<BufferPool>,
        pending: Arc<PendingStore>,
        buckets: Arc<SaeBuckets>,
        sae_binding: Arc<SaeBindingCache>,
        sdn: Option<Arc<SdnClient>>,
        qkc: Option<Arc<QkcClient>>,
        orr: Option<Arc<OrrClient>>,
        peer_client: Option<Arc<PeerHttpClient>>,
    ) -> Self {
        Self {
            cfg,
            metrics,
            pool,
            pending,
            buckets,
            sae_binding,
            sdn,
            qkc,
            orr,
            peer_client,
            admission: Admission::new(),
        }
    }

    fn self_node(&self) -> NodeId {
        NodeId::new(self.cfg.node_id.clone())
    }

    // ─── ETSI 014 ──────────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn status_for(&self, _requester: &SaeId, slave: &SaeId) -> Result<Etsi014Status> {
        let target_node = self
            .sae_binding
            .resolve(slave)
            .await
            .unwrap_or_else(|_| self.self_node());

        let stored_key_count = self.pool.for_peer(target_node.as_str()).enc.len() as u64;

        Ok(Etsi014Status {
            source_kme_id: self.cfg.node_id.clone(),
            target_kme_id: target_node.into_inner(),
            master_sae_id: String::new(),
            slave_sae_id: slave.to_string(),
            key_size: 256,
            stored_key_count,
            max_key_count: self.cfg.buffer.capacity_per_peer as u64,
            max_key_per_request: 64,
            max_key_size: 4096,
            min_key_size: 64,
            max_sae_id_count: 16,
            status_extension: None,
        })
    }

    #[instrument(skip(self, body, extra_saes_header), fields(master = %master, slave = %slave))]
    pub async fn handle_enc_keys(
        &self,
        master: &SaeId,
        slave: &SaeId,
        body: Etsi014KeyRequest,
        extra_saes_header: Option<&Value>,
    ) -> Result<Etsi014KeyContainer> {
        body.validate().map_err(|e| DkmsError::BadRequest(e.to_string()))?;

        // 1) Lista efectiva de SAEs destino (slave + additionals).
        let mut authorized: Vec<SaeId> = std::iter::once(slave.clone())
            .chain(
                body.resolved_additional_slave_sae_ids(extra_saes_header)
                    .into_iter()
                    .map(SaeId::new),
            )
            .collect();
        authorized.sort();
        authorized.dedup();

        // 2) Agrupar por DKMS destino (resolución await).
        let me = self.self_node();
        let mut groups: BTreeMap<NodeId, Vec<SaeId>> = BTreeMap::new();
        for sae in &authorized {
            let node = self.sae_binding.resolve(sae).await?;
            groups.entry(node).or_default().push(sae.clone());
        }
        let local_authorized: HashSet<SaeId> = groups
            .get(&me)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let remote_groups: Vec<(NodeId, Vec<SaeId>)> =
            groups.into_iter().filter(|(n, _)| n != &me).collect();

        if remote_groups.len() > self.cfg.request.max_concurrent_peers {
            return Err(DkmsError::BadRequest(format!(
                "too many destination DKMSs ({}), max {}",
                remote_groups.len(),
                self.cfg.request.max_concurrent_peers
            )));
        }

        // 3) Coste y admisión.
        let size_bytes = (body.size as u64).div_ceil(8);
        let num_dkms_destinations = remote_groups.len() as u64;
        let cost = compute_cost(
            size_bytes,
            body.number as u64,
            num_dkms_destinations,
            self.cfg.sae.token_unit_bytes,
        );
        self.buckets
            .try_consume(master, cost)
            .map_err(|available| DkmsError::RateLimited {
                sae: master.clone(),
                requested: cost,
                available,
            })?;

        // 4) Generar las `number` claves de sesión.
        let session_keys = generate_session_keys(body.number, size_bytes as usize);

        // 5) Empaquetar para los SAEs locales (multicast con destino local).
        if !local_authorized.is_empty() {
            for (_uuid, kid, k) in &session_keys {
                self.pending.insert(
                    kid.clone(),
                    master.clone(),
                    local_authorized.clone(),
                    k.to_vec(),
                    None,
                );
            }
        }

        // 6) Distribuir cada (K, peer DKMS) por el transporte elegido en
        //    config (ETSI 020 HTTP/2 o ORR gRPC). Particionamos primero
        //    para no consumir transport keys del pool QKC en el caso ORR
        //    (donde la cripto la dan onion + OTP por enlace QKC↔QKC).
        let (http_peers, orr_peers): (Vec<_>, Vec<_>) = remote_groups
            .iter()
            .partition(|(node, _)| {
                self.cfg
                    .peers
                    .get(node.as_str())
                    .map(|p| p.transport == PeerTransport::Http)
                    .unwrap_or(true) // default = http si no hay entry
            });

        // 6a) Construcción de envelopes ETSI 020 (síncrono — el pop del
        //     buffer_enc no debe quedar a medias por un error tardío).
        let mut envelopes: Vec<(NodeId, Etsi020ExtKeyContainer)> =
            Vec::with_capacity(http_peers.len());
        let mut transport_keys_consumed = 0usize;
        for (peer_node, peer_saes) in http_peers.iter() {
            let envelope =
                self.build_ext_keys_envelope(master, peer_node, peer_saes, &session_keys)?;
            transport_keys_consumed += envelope.keys.len();
            envelopes.push(((*peer_node).clone(), envelope));
        }

        // 6b) Futuros de envío. Un Vec mixto: cada futuro devuelve
        //     `Result<NodeId, DkmsError>` para que el join_all sea
        //     uniforme. Las dos ramas comparten política de error.
        type SendFuture =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<NodeId>> + Send>>;
        let mut futures: Vec<SendFuture> = Vec::with_capacity(http_peers.len() + orr_peers.len());

        for (peer_node, envelope) in envelopes.into_iter() {
            let pc = self.peer_client.clone().ok_or_else(|| {
                DkmsError::BadRequest(format!(
                    "peer dkms {peer_node} requires http transport but peer_client is disabled"
                ))
            })?;
            let peer_cfg = self.cfg.peers.get(peer_node.as_str()).cloned().ok_or_else(|| {
                DkmsError::BadRequest(format!("peer dkms {} not configured", peer_node))
            })?;
            let peer_id_str = peer_node.to_string();
            let send_timeout = Duration::from_millis(self.cfg.request.peer_send_timeout_ms);
            futures.push(Box::pin(async move {
                let r = tokio::time::timeout(
                    send_timeout,
                    pc.send_ext_keys(&peer_id_str, &peer_cfg, &envelope),
                )
                .await;
                match r {
                    Ok(Ok(_ack)) => Ok(peer_node),
                    Ok(Err(e)) => Err(e),
                    Err(_) => Err(DkmsError::PeerAckTimeout { peer: peer_id_str }),
                }
            }));
        }

        for (peer_node, peer_saes) in orr_peers.into_iter() {
            let orr = self.orr.clone().ok_or_else(|| DkmsError::PeerOrrMisconfig {
                peer: peer_node.to_string(),
                missing: "southbound.orr_endpoint",
            })?;
            let peer_cfg = self.cfg.peers.get(peer_node.as_str()).cloned().ok_or_else(|| {
                DkmsError::BadRequest(format!("peer dkms {} not configured", peer_node))
            })?;
            let dest_orr_id = peer_cfg.orr_id.clone().ok_or_else(|| {
                DkmsError::PeerOrrMisconfig {
                    peer: peer_node.to_string(),
                    missing: "peer.orr_id",
                }
            })?;
            let max_hops = peer_cfg
                .max_hops
                .unwrap_or(self.cfg.southbound.default_max_hops);
            let orr_path_hint = peer_cfg.orr_path.clone();
            let initiator = master.to_string();
            let key_bits = body.size;
            let request_id = Uuid::new_v4().to_string();
            // Una send_key por (K × target_sae): el receptor maneja
            // (key_id, sae_destination) → PendingStore.
            let pairs: Vec<(KeyId, Vec<u8>, SaeId)> = session_keys
                .iter()
                .flat_map(|(_uuid, kid, k_bytes)| {
                    peer_saes
                        .iter()
                        .map(move |sae| (kid.clone(), k_bytes.to_vec(), sae.clone()))
                })
                .collect();
            let peer_node_owned = peer_node.clone();
            let send_timeout = Duration::from_millis(self.cfg.request.peer_send_timeout_ms);
            futures.push(Box::pin(async move {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let mut subfutures = Vec::with_capacity(pairs.len());
                for (kid, k_bytes, sae) in pairs {
                    let mut header: BTreeMap<String, String> = BTreeMap::new();
                    header.insert(HDR_KEY_ID.into(), kid.to_string());
                    header.insert(HDR_SAE_ORIGIN.into(), initiator.clone());
                    header.insert(HDR_SAE_DESTINATION.into(), sae.to_string());
                    header.insert(HDR_KEY_SIZE_BITS.into(), key_bits.to_string());
                    header.insert(HDR_REQUEST_ID.into(), request_id.clone());
                    header.insert(HDR_TIMESTAMP_MS.into(), now_ms.to_string());
                    if let Some(path) = &orr_path_hint {
                        // Hint para modos onion >=2 / -1: el ORR lee
                        // app_header["orr_path"] para armar la cebolla.
                        // Cuando la SDN exista, se calculará allí en
                        // vez de venir hardcoded en config.
                        header.insert("orr_path".into(), path.clone());
                    }
                    let _ = HDR_FLOW_ID; // reservado para routing por flujo; no se setea hoy.
                    let orr_cloned = orr.clone();
                    let dest = dest_orr_id.clone();
                    subfutures.push(async move {
                        tokio::time::timeout(send_timeout, orr_cloned.send_key(&dest, k_bytes, header, max_hops)).await
                    });
                }
                for r in join_all(subfutures).await {
                    match r {
                        Ok(Ok(_resp)) => {}
                        Ok(Err(e)) => {
                            return Err(DkmsError::OrrSendFailed {
                                peer: peer_node_owned.to_string(),
                                source: anyhow::anyhow!(e.to_string()),
                            });
                        }
                        Err(_) => {
                            return Err(DkmsError::PeerAckTimeout {
                                peer: peer_node_owned.to_string(),
                            });
                        }
                    }
                }
                Ok(peer_node_owned)
            }));
        }

        let results = join_all(futures).await;
        let failures: Vec<&DkmsError> =
            results.iter().filter_map(|r| r.as_ref().err()).collect();
        if !failures.is_empty() {
            for e in &failures {
                error!(error = %e, "key distribution failure");
            }
            // Reembolso y retracción local.
            self.buckets.refund(master, cost);
            for (_uuid, kid, _k) in &session_keys {
                self.pending.force_remove(kid);
            }
            debug!(
                count = transport_keys_consumed,
                "transport keys lost to failed distribution"
            );
            // Devolver el primer error como representativo (502/upstream).
            return Err(DkmsError::PeerUnreachable {
                peer: failures
                    .first()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "unknown".into()),
                source: anyhow::anyhow!("one or more peer DKMSs failed during key distribution"),
            });
        }

        // 7) Responder al master con K's en claro.
        let keys = session_keys
            .iter()
            .map(|(uuid, _kid, k)| Etsi014Key {
                key_id: *uuid,
                key_id_extension: None,
                key: Base64Bytes::new(k.to_vec()),
                key_extension: None,
            })
            .collect();
        Ok(Etsi014KeyContainer {
            keys,
            key_container_extension: None,
        })
    }

    #[instrument(skip(self, body), fields(requester = %requester, master = %master))]
    pub async fn handle_dec_keys(
        &self,
        requester: &SaeId,
        master: &SaeId,
        body: Etsi014KeyIDs,
    ) -> Result<Etsi014KeyContainer> {
        let _ = master; // master_SAE_ID viaja por auditoría; la autoría real la fija mTLS.
        let mut out = Vec::with_capacity(body.key_ids.len());
        for kid in &body.key_ids {
            let key_id = KeyId::new(kid.key_id.to_string());
            let (material, _initiator) = self.pending.take_for_sae(&key_id, requester)?;
            out.push(Etsi014Key {
                key_id: kid.key_id,
                key_id_extension: None,
                key: Base64Bytes::new(material.to_vec()),
                key_extension: None,
            });
        }
        Ok(Etsi014KeyContainer {
            keys: out,
            key_container_extension: None,
        })
    }

    // ─── ETSI 020 (entrante) ───────────────────────────────────────────

    #[instrument(skip(self, body), fields(peer = %peer))]
    pub async fn handle_incoming_ext_keys(
        &self,
        peer: &NodeId,
        body: Etsi020ExtKeyContainer,
    ) -> Result<Etsi020ExtKeyAckContainer> {
        body.validate().map_err(|e| DkmsError::BadRequest(e.to_string()))?;

        let initiator = SaeId::new(body.initiator_sae_id.clone());
        let authorized: HashSet<SaeId> = body
            .target_sae_ids
            .iter()
            .map(|s| SaeId::new(s.clone()))
            .collect();

        let ttl = body
            .extension_mandatory
            .as_ref()
            .and_then(|m| m.get("ttl_seconds"))
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs);

        let peer_buffers = self.pool.for_peer(peer.as_str());
        let mut ack_ids: Vec<Etsi020KeyID> = Vec::with_capacity(body.keys.len());

        for k in &body.keys {
            let transport_key_id = k
                .extension
                .as_ref()
                .and_then(|m| m.get("transport_key_id"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    DkmsError::BadRequest(
                        "Etsi020Key.extension.transport_key_id missing".into(),
                    )
                })?
                .to_owned();
            let tk_id = KeyId::new(transport_key_id.clone());
            let tk = peer_buffers
                .dec
                .take_by_id(&tk_id)
                .ok_or_else(|| DkmsError::TransportKeyMissing {
                    peer: peer.to_string(),
                    key_id: transport_key_id,
                })?;

            let plaintext = unwrap_session_key(tk.bytes.as_slice(), k.value.as_ref())
                .map_err(DkmsError::Crypto)?;
            self.pending.insert(
                KeyId::new(k.key_id.to_string()),
                initiator.clone(),
                authorized.clone(),
                plaintext,
                ttl,
            );
            ack_ids.push(Etsi020KeyID::new(k.key_id));
        }

        // ETSI 020 espera un único `target_sae_id` por ACK; con multicast
        // tomamos el primero, los demás quedan implícitos por `key_ids`.
        let target_sae_id = body.target_sae_ids.first().cloned().unwrap_or_default();
        Ok(Etsi020ExtKeyAckContainer {
            key_ids: ack_ids,
            ack_status: Etsi020AckStatus::Relayed,
            initiator_sae_id: body.initiator_sae_id,
            target_sae_id,
            message: None,
            extension: None,
        })
    }

    // ─── Helpers internos ──────────────────────────────────────────────

    fn build_ext_keys_envelope(
        &self,
        master: &SaeId,
        peer_node: &NodeId,
        peer_saes: &[SaeId],
        session_keys: &[(Uuid, KeyId, Zeroizing<Vec<u8>>)],
    ) -> Result<Etsi020ExtKeyContainer> {
        let peer_buffers = self.pool.for_peer(peer_node.as_str());
        let mut etsi_keys = Vec::with_capacity(session_keys.len());
        for (uuid, _kid, k_bytes) in session_keys {
            let tk = peer_buffers
                .enc
                .pop_oldest()
                .ok_or_else(|| DkmsError::TransportBufferEmpty {
                    peer: peer_node.to_string(),
                })?;
            if tk.bytes.len() < TRANSPORT_KEY_BYTES {
                return Err(DkmsError::Crypto(format!(
                    "transport key too short ({} bytes, need {})",
                    tk.bytes.len(),
                    TRANSPORT_KEY_BYTES
                )));
            }
            let ciphertext =
                wrap_session_key(tk.bytes.as_slice(), k_bytes.as_slice()).map_err(DkmsError::Crypto)?;
            let mut ext = serde_json::Map::new();
            ext.insert(
                "transport_key_id".to_owned(),
                Value::String(tk.id.to_string()),
            );
            etsi_keys.push(Etsi020Key {
                key_id: *uuid,
                value: Base64Bytes::new(ciphertext),
                extension: Some(ext),
            });
        }
        let mut extension_mandatory = serde_json::Map::new();
        extension_mandatory.insert(
            "ttl_seconds".to_owned(),
            Value::from(self.cfg.pending.default_ttl_secs),
        );

        Ok(Etsi020ExtKeyContainer {
            keys: etsi_keys,
            initiator_sae_id: master.to_string(),
            target_sae_ids: peer_saes.iter().map(|s| s.to_string()).collect(),
            ack_callback_url: format!("https://{}/kmapi/v1/ext_keys/ack", self.cfg.node_id),
            extension_mandatory: Some(extension_mandatory),
            extension_optional: None,
        })
    }

    pub async fn run_background_tasks(self) -> Result<()> {
        info!("dkms: background tasks started");
        let pending = self.pending.clone();
        let sweep_period = self.cfg.pending.sweep_interval_secs;
        tokio::spawn(crate::state::pending::sweeper_task(pending, sweep_period));

        // Pump del transporte ORR. Si el cliente está conectado, abre
        // `StreamDeliveries` y vuelca cada `DeliveredMessage` al
        // `PendingStore` como una clave entregada. Esto es el ETSI 020
        // pero sobre transporte ORR↔QKC en vez de HTTP/2.
        if self.orr.is_some() {
            let pump = self.clone();
            tokio::spawn(async move {
                pump.run_orr_delivery_pump().await;
            });
        }

        // TODO: refill de buffer_enc desde QKC (Reserve→fetch material)
        // y suscripción a topology updates de SDN para invalidar
        // sae_binding cache.
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
        }
    }

    /// Bucle que mantiene abierto el `StreamDeliveries` del ORR
    /// co-localizado. Reconecta con backoff si la suscripción cae.
    async fn run_orr_delivery_pump(&self) {
        let Some(orr) = self.orr.clone() else { return };
        let subscriber_id = format!("dkms-{}", self.cfg.node_id);
        let mut backoff_ms: u64 = 250;
        loop {
            match orr.subscribe_deliveries(&subscriber_id).await {
                Ok(mut stream) => {
                    info!(subscriber = %subscriber_id, "orr deliveries pump connected");
                    backoff_ms = 250;
                    while let Some(item) = stream.message().await.transpose() {
                        match item {
                            Ok(msg) => {
                                if let Err(e) = self.handle_orr_delivery(msg).await {
                                    warn!(error = %e, "orr delivery handle failed");
                                }
                            }
                            Err(s) => {
                                warn!(status = %s, "orr deliveries stream broken; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => warn!(error = %e, "orr deliveries subscribe failed; retrying"),
            }
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(5_000);
        }
    }

    /// Procesa un `DeliveredMessage` que llega vía ORR. Espera que el
    /// `app_header` traiga las claves canónicas que define
    /// `southbound::orr` (`HDR_*`). El `payload` son los bytes crudos de
    /// la clave QKD.
    async fn handle_orr_delivery(
        &self,
        msg: common::proto::orr::v1::DeliveredMessage,
    ) -> Result<()> {
        let app = &msg.app_header;
        let key_id_str = app
            .get(HDR_KEY_ID)
            .ok_or_else(|| DkmsError::BadRequest(format!("orr delivery missing {HDR_KEY_ID}")))?;
        let initiator = app
            .get(HDR_SAE_ORIGIN)
            .ok_or_else(|| DkmsError::BadRequest(format!("orr delivery missing {HDR_SAE_ORIGIN}")))?
            .clone();
        let dest_sae = app
            .get(HDR_SAE_DESTINATION)
            .ok_or_else(|| {
                DkmsError::BadRequest(format!("orr delivery missing {HDR_SAE_DESTINATION}"))
            })?
            .clone();
        // Estos campos son informativos hoy — los leemos para validar
        // y loguear, no se usan más allá.
        let key_size = app.get(HDR_KEY_SIZE_BITS).and_then(|v| v.parse::<u32>().ok());
        let request_id = app.get(HDR_REQUEST_ID).cloned();
        let _flow_id = app.get(HDR_FLOW_ID).cloned();
        let _ts_ms = app.get(HDR_TIMESTAMP_MS).cloned();

        if let Some(bits) = key_size {
            let expected = (bits as usize).div_ceil(8);
            if msg.payload.len() != expected {
                return Err(DkmsError::BadRequest(format!(
                    "orr delivery {key_id_str}: payload {} bytes, header says {bits} bits ({expected} bytes)",
                    msg.payload.len()
                )));
            }
        }

        let key_id = KeyId::new(key_id_str.clone());
        let initiator_sae = SaeId::new(initiator);
        let mut authorized: HashSet<SaeId> = HashSet::new();
        authorized.insert(SaeId::new(dest_sae));

        self.pending.insert(
            key_id,
            initiator_sae,
            authorized,
            msg.payload,
            None, // TTL por defecto del config
        );
        debug!(
            key_id = %key_id_str,
            request_id = ?request_id,
            "orr delivery → pending store",
        );
        Ok(())
    }
}

// ─── Crypto helpers ─────────────────────────────────────────────────────

fn generate_session_keys(n: u32, size_bytes: usize) -> Vec<(Uuid, KeyId, Zeroizing<Vec<u8>>)> {
    let mut out = Vec::with_capacity(n as usize);
    let mut rng = OsRng;
    for _ in 0..n {
        let mut k = Zeroizing::new(vec![0u8; size_bytes]);
        rng.fill_bytes(&mut k);
        let uuid = Uuid::new_v4();
        let kid = KeyId::new(uuid.to_string());
        out.push((uuid, kid, k));
    }
    out
}

/// Envuelve `k` con AEAD usando los primeros 32 bytes de `transport`.
/// El nonce (12 bytes) se prepende al ciphertext: `nonce || ct || tag`.
fn wrap_session_key(transport: &[u8], k: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if transport.len() < TRANSPORT_KEY_BYTES {
        return Err(format!("transport key too short ({} bytes)", transport.len()));
    }
    let cipher = ChaCha20Poly1305::new_from_slice(&transport[..TRANSPORT_KEY_BYTES])
        .map_err(|e| e.to_string())?;
    let mut nonce_bytes = [0u8; NONCE_BYTES];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, k).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(NONCE_BYTES + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn unwrap_session_key(transport: &[u8], wire: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if wire.len() < NONCE_BYTES + 16 {
        return Err("ciphertext too short".into());
    }
    if transport.len() < TRANSPORT_KEY_BYTES {
        return Err(format!("transport key too short ({} bytes)", transport.len()));
    }
    let cipher = ChaCha20Poly1305::new_from_slice(&transport[..TRANSPORT_KEY_BYTES])
        .map_err(|e| e.to_string())?;
    let (nonce_bytes, ct) = wire.split_at(NONCE_BYTES);
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher.decrypt(nonce, ct).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_unwrap_roundtrip() {
        let mut transport = vec![0u8; TRANSPORT_KEY_BYTES];
        OsRng.fill_bytes(&mut transport);
        let k = b"this is a session key K".to_vec();
        let wire = wrap_session_key(&transport, &k).unwrap();
        let pt = unwrap_session_key(&transport, &wire).unwrap();
        assert_eq!(pt, k);
    }
}
