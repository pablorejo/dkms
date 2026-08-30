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
//!    con sus `target_sae_ids`, cifrando cada `K` con **una** clave de
//!    transporte fresca de `buffer_enc[peer]` por **OTP puro** (XOR).
//!    El buffer se rellena en background por el generator vía ORR — el
//!    transporte SAE-key NO usa ORR.
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
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::Duration,
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
    config::DkmsConfig,
    control::{BatchedAckClient, FlowStats, Generator, SaeBufferBuckets},
    demand_tracker::{DemandTracker, SharedDemandTracker},
    error::{DkmsError, Result},
    peer_client::PeerHttpClient,
    peers::PeerIncarnations,
    peers::PeerRegistry,
    sae_binding::SaeBindingCache,
    southbound::{
        orr::{
            HDR_ACK_ENDPOINT, HDR_INCARNATION, HDR_KEY_ID, HDR_KEY_SIZE_BITS, HDR_MSG_TYPE,
            HDR_SAE_ORIGIN, MSG_TYPE_DKMS_BUFFER,
        },
        OrrClient, SdnClient,
    },
    state::{buffer::TransportKey, BufferPool, PendingStore},
    token_bucket::{compute_cost, SaeBuckets},
};

/// Tamaño mínimo en bytes de la clave de transporte (la salida QKD por
/// defecto, alineada con `generator.key_size_bytes = 32`). Se usa para
/// validar que la clave OTP cubre al menos los tamaños de session_key
/// que un SAE puede pedir típicamente (256 bits).
const TRANSPORT_KEY_BYTES: usize = 32;

// Límites ETSI-014 de este KME. Son a la vez lo que `/status` publica y lo que
// `check_request_limits` exige: un solo sitio, para que no puedan divergir.
const MAX_KEY_PER_REQUEST: u32 = 64;
const MAX_KEY_SIZE_BITS: u32 = 4096;
const MIN_KEY_SIZE_BITS: u32 = 64;
const MAX_SAE_ID_COUNT: usize = 16;

/// Comprueba una petición ETSI-014 contra los límites que este KME **anuncia**
/// en `/status`.
///
/// Anunciarlos y no comprobarlos es peor que no anunciarlos: el SAE los lee,
/// se fía y pide dentro de lo permitido, mientras el KME acepta cualquier
/// cosa. Sin esta comprobación (verificado en el testbed el 2026-08-02),
/// `number` era libre —una petición podía vaciar el buffer de golpe—, un
/// `size` de 7 bits devolvía en silencio una clave de 8, distinta de la
/// pedida y sin forma de que el SAE lo note, y un `size` disparatado acababa
/// en un 500 en lugar de en un rechazo limpio.
fn check_request_limits(body: &Etsi014KeyRequest, n_saes: usize) -> Result<()> {
    if body.number > MAX_KEY_PER_REQUEST {
        return Err(DkmsError::BadRequest(format!(
            "number {} exceeds max_key_per_request {MAX_KEY_PER_REQUEST}",
            body.number
        )));
    }
    if !body.size.is_multiple_of(8) {
        return Err(DkmsError::BadRequest(format!(
            "size {} is not a multiple of 8 bits",
            body.size
        )));
    }
    if body.size < MIN_KEY_SIZE_BITS || body.size > MAX_KEY_SIZE_BITS {
        return Err(DkmsError::BadRequest(format!(
            "size {} outside [{MIN_KEY_SIZE_BITS}, {MAX_KEY_SIZE_BITS}] bits",
            body.size
        )));
    }
    if n_saes > MAX_SAE_ID_COUNT {
        return Err(DkmsError::BadRequest(format!(
            "{n_saes} destination SAEs exceeds max_SAE_ID_count {MAX_SAE_ID_COUNT}"
        )));
    }
    Ok(())
}

/// ¿`requester` es un SAE que este DKMS (`node_id`) declara servir?
/// La fuente es el mapa `sae_bindings` (SAE → node_id del DKMS donde
/// reside): sirve al SAE si hay una entrada suya que apunta a mí. Es la
/// misma lista que se anuncia a la SDN.
fn sae_served_locally(
    sae_bindings: &HashMap<String, String>,
    node_id: &str,
    requester: &SaeId,
) -> bool {
    sae_bindings
        .iter()
        .any(|(sae, node)| node == node_id && sae.as_str() == requester.as_str())
}

#[derive(Clone)]
pub struct DkmsService {
    pub cfg: Arc<DkmsConfig>,
    pub metrics: Metrics,

    pub pool: Arc<BufferPool>,
    pub pending: Arc<PendingStore>,
    pub buckets: Arc<SaeBuckets>,
    pub sae_binding: Arc<SaeBindingCache>,

    /// Peers DKMS, actualizables por la SDN. Sustituye a leer `cfg.peers`
    /// directamente: así un DKMS que entra en la red después existe para los
    /// que ya estaban. Ver [`crate::peers`].
    pub peers: Arc<PeerRegistry>,

    pub sdn: Option<Arc<SdnClient>>,
    /// Cliente gRPC al ORR co-localizado. Si está presente, el DKMS
    /// puede usar el transporte ORR↔QKC (binario sobre TCP) como
    /// alternativa al HTTP/2 ETSI 020 entre DKMSs. Se cablea como
    /// `Option` para no romper despliegues sin ORR.
    pub orr: Option<Arc<OrrClient>>,
    /// Cliente HTTP/2 ETSI 020 hacia DKMSs peer. `None` en despliegues
    /// donde todos los peers usan `transport = "orr"` (no se intenta
    /// abrir un Reqwest TLS context si nadie lo usa).
    pub peer_client: Option<Arc<PeerHttpClient>>,

    /// Generator que llena `buffer_enc[peer]` al ritmo del SDN. `None` en
    /// despliegues HTTP/2 puros que prefieran el refill clásico
    /// (importado desde el ETSI 020 entrante).
    pub generator: Option<Arc<Generator>>,
    /// Cliente para mandar ACKs a peers (cuando éste DKMS recibe un
    /// `DKMS_BUFFER` por ORR). Si `None`, el delivery pump no manda ACK
    /// (los peers entonces verán expiraciones del ack_pending).
    pub ack_client: Option<Arc<BatchedAckClient>>,
    /// Contadores del ciclo emit → deliver → ACK, compartidos con el
    /// Generator y el socket de ACK. Aquí se alimenta el lado receptor
    /// (`recv`, `ack_*`). Ver [`crate::control::flow_stats`].
    pub flow: Arc<FlowStats>,
    /// Token buckets per `(peer, sae)` con límites dinámicos
    /// (refill = link_capacity/N_SAEs, capacity = occupancy/N_SAEs).
    /// Si `None`, se usa el legacy `SaeBuckets` por master-SAE en su
    /// lugar (despliegues sin Generator/ORR).
    pub sae_buffer_buckets: Option<Arc<SaeBufferBuckets>>,

    /// Estado de admisión — consultado por el `AdmissionLayer` del HTTP y
    /// pilotado por el RPC `Drain` del plano gRPC.
    pub admission: Arc<Admission>,

    /// EWMA-smoothed SAE demand per peer DKMS. Updated in
    /// `handle_enc_keys` **before** bucket admission (so it reflects
    /// what SAEs request, not what the buckets let through), and
    /// reported periodically to the SDN's `POST /demand` endpoint by
    /// the Generator's demand loop. Used by the MCMCF-λ solver as
    /// `δ_k` (drain rate per commodity).
    pub demand_tracker: SharedDemandTracker,

    /// Última ejecución conocida de cada peer. Ver
    /// [`crate::peers::PeerIncarnations`] y
    /// [`DkmsService::forget_material_of_previous_incarnation`].
    peer_incarnations: Arc<PeerIncarnations>,

    /// Capa extremo a extremo DKMS↔DKMS del material de transporte. La
    /// comparten el generador (sella), el pump de entregas (abre) y el
    /// servidor del plano peer (responde acuerdos). Ver [`crate::e2e`].
    pub e2e: Arc<crate::e2e::E2e>,
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
        peers: Arc<PeerRegistry>,
        sdn: Option<Arc<SdnClient>>,
        orr: Option<Arc<OrrClient>>,
        peer_client: Option<Arc<PeerHttpClient>>,
    ) -> Self {
        let e2e = Arc::new(crate::e2e::E2e::new(
            cfg.node_id.clone(),
            cfg.transport_e2e.clone(),
            peer_client.clone(),
            peers.clone(),
        ));
        Self {
            peer_incarnations: Arc::new(PeerIncarnations::new()),
            e2e,
            cfg,
            metrics,
            pool,
            pending,
            buckets,
            sae_binding,
            peers,
            sdn,
            orr,
            peer_client,
            generator: None,
            ack_client: None,
            // Se crea aquí y `main.rs` la pasa al Generator y al
            // BatchedAckClient, que nacen después: así los dos sentidos
            // del camino de claves cuentan sobre la misma tabla.
            flow: Arc::new(FlowStats::new()),
            sae_buffer_buckets: None,
            admission: Admission::new(),
            demand_tracker: Arc::new(DemandTracker::default()),
        }
    }

    /// Inyecta el Generator, AckClient y SaeBufferBuckets. Llamado desde
    /// `main.rs` tras crear el servicio. `Option` para permitir tests
    /// que no usan el plano de buffers compartidos.
    pub fn set_control(
        &mut self,
        generator: Option<Arc<Generator>>,
        ack_client: Option<Arc<BatchedAckClient>>,
        sae_buffer_buckets: Option<Arc<SaeBufferBuckets>>,
    ) {
        self.generator = generator;
        self.ack_client = ack_client;
        self.sae_buffer_buckets = sae_buffer_buckets;
    }

    fn self_node(&self) -> NodeId {
        NodeId::new(self.cfg.node_id.clone())
    }

    /// Autoriza que el SAE autenticado por mTLS sea uno de los que este
    /// DKMS declara servir. La lista es `sae_bindings` cuyo valor es mi
    /// `node_id` — la misma que anuncio a la SDN. Cierra el hueco de que
    /// cualquier cert válido pudiera pedir/recuperar claves en nombre de
    /// cualquier SAE: en ETSI-014 el que llama (master en enc/status,
    /// slave en dec) es siempre un SAE local a este KME.
    ///
    /// Desactivable con `sae.enforce_authorization = false` para
    /// despliegues con pertenencia SAE→DKMS puramente dinámica vía SDN.
    fn authorize_local_sae(&self, requester: &SaeId) -> Result<()> {
        if !self.cfg.sae.enforce_authorization {
            return Ok(());
        }
        if sae_served_locally(&self.cfg.sae_bindings, &self.cfg.node_id, requester) {
            Ok(())
        } else {
            // Con cert válido pero SAE no servido: en campaña, si esto aparece
            // para SAEs que DEBERÍAN estar servidos, el sae_bindings del nodo
            // está mal renderizado (mirar el default.toml generado).
            warn!(
                sae = %requester,
                node = %self.cfg.node_id,
                bindings = self.cfg.sae_bindings.len(),
                "auth.reject: SAE autenticado pero no servido por este nodo (404)"
            );
            Err(DkmsError::UnknownSae(requester.clone()))
        }
    }

    // ─── ETSI 014 ──────────────────────────────────────────────────────

    #[instrument(skip(self))]
    pub async fn status_for(&self, requester: &SaeId, slave: &SaeId) -> Result<Etsi014Status> {
        self.authorize_local_sae(requester)?;
        let target_node = self
            .sae_binding
            .resolve(slave)
            .await
            .unwrap_or_else(|_| self.self_node());

        let stored_key_count = self.pool.for_peer(target_node.as_str()).enc_len() as u64;

        Ok(Etsi014Status {
            source_kme_id: self.cfg.node_id.clone(),
            target_kme_id: target_node.into_inner(),
            // El SAE que pregunta ES el maestro de esta consulta; su identidad
            // viene del cert de cliente, así que ya la tenemos verificada.
            // Devolverla vacía era una desviación de ETSI-014 sin motivo.
            master_sae_id: requester.to_string(),
            slave_sae_id: slave.to_string(),
            key_size: 256,
            stored_key_count,
            max_key_count: self.cfg.buffer.capacity_per_peer as u64,
            max_key_per_request: MAX_KEY_PER_REQUEST,
            max_key_size: MAX_KEY_SIZE_BITS,
            min_key_size: MIN_KEY_SIZE_BITS,
            max_sae_id_count: MAX_SAE_ID_COUNT as u32,
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
        self.authorize_local_sae(master)?;
        body.validate()
            .map_err(|e| DkmsError::BadRequest(e.to_string()))?;

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

        // Después del dedup: lo que limita `max_SAE_ID_count` es el número de
        // destinos reales, no cuántas veces los repita el cliente.
        check_request_limits(&body, authorized.len())?;

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
        //
        // El nuevo modelo (SaeBufferBuckets) carga un bucket POR cada
        // peer DKMS destino con un coste `cost_per_peer`. El bucket es
        // por `(peer, sae)` y sus límites se recalculan en cada admit:
        //   refill   = link_capacity / N_active_SAEs
        //   capacity = max(buffer_occupancy / N_active_SAEs, refill × window)
        //
        // Esto reparte el ancho de banda de cada buffer entre los SAEs
        // que lo están usando, evitando que un SAE agresivo lo monopolice.
        //
        // Fallback legacy: si no hay SaeBufferBuckets cableado
        // (despliegue sin Generator/ORR) se usa el bucket plano por
        // master-SAE de `SaeBuckets`.
        let size_bytes = (body.size as u64).div_ceil(8);
        let cost_per_peer = compute_cost(
            size_bytes,
            body.number as u64,
            1,
            self.cfg.sae.token_unit_bytes,
        ) as f64;
        let remote_peer_ids: Vec<String> =
            remote_groups.iter().map(|(n, _)| n.to_string()).collect();

        // Security-level admission. Resolve the requested level (validates a
        // mandatory `security_level` extension — an unknown token → 4xx). A
        // `strict_qkd` request to a peer the SDN reports as QKD-unreachable
        // cannot be served (no QKD-grade key path exists), so reject it up
        // front rather than hand back a weaker PQC-grade key. Unknown
        // connectivity (bootstrap, or no generator wired) is permissive —
        // enforcement kicks in once the first `/rate` poll lands.
        let requested_level =
            crate::security_level::requested(&body).map_err(DkmsError::BadRequest)?;
        for peer_id in &remote_peer_ids {
            let level = requested_level.unwrap_or_else(|| self.cfg.security_level_for(peer_id));
            if level == common::security::SecurityLevel::StrictQkd
                && self
                    .generator
                    .as_ref()
                    .and_then(|g| g.qkd_available(peer_id))
                    == Some(false)
            {
                return Err(DkmsError::BadRequest(format!(
                    "strict_qkd requested but SDN reports no QKD path to dkms {peer_id}"
                )));
            }
        }

        // Record SAE demand for the MCMCF-λ solver — keys/s per
        // (self, peer) commodity. Done BEFORE the bucket admission
        // call below, so a rate-limited request still shows up as
        // demand to the SDN. Without this the system self-starves:
        // limited buffers report δ≈0 → SDN allocates them less →
        // they stay limited.
        if !remote_peer_ids.is_empty() {
            let now_ms = chrono::Utc::now().timestamp_millis();
            let demanded = cost_per_peer as u64;
            for peer_id in &remote_peer_ids {
                self.demand_tracker.record(peer_id, demanded, now_ms);
            }
        }

        if let Some(sbb) = self.sae_buffer_buckets.as_ref() {
            if !remote_peer_ids.is_empty() {
                if let Err(fail) = sbb.try_admit_many(&remote_peer_ids, master, cost_per_peer) {
                    return Err(DkmsError::RateLimited {
                        sae: master.clone(),
                        requested: fail.requested as u64,
                        available: fail.available as u64,
                    });
                }
            }
        } else {
            // Legacy: master-SAE only.
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
        }

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

        // 6) Distribuir cada (K, peer DKMS) **únicamente** por HTTP ETSI
        //    020 DKMS↔DKMS. La session_key viaja cifrada por OTP con una
        //    clave fresca del `buffer_enc[peer]`. Ese buffer se rellena
        //    en background por el generator vía ORR — pero para la SAE
        //    key-delivery NO se usa ORR como transporte. Esta separación
        //    deja al ORR como dispositivo de bombeo de material QKD y a
        //    HTTP+OTP como capa de servicio a SAEs (información-
        //    teóricamente segura porque OTP con clave QKD).
        let mut envelopes: Vec<(NodeId, Etsi020ExtKeyContainer)> =
            Vec::with_capacity(remote_groups.len());
        let mut transport_keys_consumed = 0usize;
        for (peer_node, peer_saes) in remote_groups.iter() {
            // Grade preference to serve this peer = the request's security
            // level (per-request override, else per-peer/global default).
            let level =
                requested_level.unwrap_or_else(|| self.cfg.security_level_for(peer_node.as_str()));
            let envelope = self.build_ext_keys_envelope(
                master,
                peer_node,
                peer_saes,
                &session_keys,
                level.serve_pref(),
            )?;
            transport_keys_consumed += envelope.keys.len();
            envelopes.push((peer_node.clone(), envelope));
        }

        type SendFuture =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<NodeId>> + Send>>;
        let mut futures: Vec<SendFuture> = Vec::with_capacity(envelopes.len());

        for (peer_node, envelope) in envelopes.into_iter() {
            let pc = self.peer_client.clone().ok_or_else(|| {
                DkmsError::BadRequest(format!(
                    "peer dkms {peer_node} ETSI020 HTTP peer_client missing — \
                     orchestator misconfig (tls.cert_path/peer_dkms_ca required)"
                ))
            })?;
            let peer_cfg = self.peers.get(peer_node.as_str()).ok_or_else(|| {
                DkmsError::BadRequest(format!("peer dkms {peer_node} not configured"))
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

        let results = join_all(futures).await;
        let failures: Vec<&DkmsError> = results.iter().filter_map(|r| r.as_ref().err()).collect();
        if !failures.is_empty() {
            for e in &failures {
                error!(error = %e, "key distribution failure");
                self.resync_transport_buffer_if_stale(e);
            }
            // Reembolso y retracción local.
            if let Some(sbb) = self.sae_buffer_buckets.as_ref() {
                sbb.refund_many(&remote_peer_ids, master, cost_per_peer);
            } else {
                // Legacy path: cost = cost_per_peer × num_dkms_destinations,
                // que es lo que se consumió en la rama legacy.
                let legacy_cost = cost_per_peer as u64 * (remote_peer_ids.len() as u64).max(1);
                self.buckets.refund(master, legacy_cost);
            }
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
        self.authorize_local_sae(requester)?;
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
        body.validate()
            .map_err(|e| DkmsError::BadRequest(e.to_string()))?;

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
                    DkmsError::BadRequest("Etsi020Key.extension.transport_key_id missing".into())
                })?
                .to_owned();
            let tk_id = KeyId::new(transport_key_id.clone());
            let tk = peer_buffers.dec.take_by_id(&tk_id).ok_or_else(|| {
                DkmsError::TransportKeyMissing {
                    peer: peer.to_string(),
                    key_id: transport_key_id,
                }
            })?;

            let plaintext = unwrap_session_key(tk.bytes.as_slice(), k.value.as_ref())
                .map_err(DkmsError::Crypto)?;

            // Comprobar la huella ANTES de guardar y de acusar recibo. Sin esto
            // una clave de transporte divergente entrega a los dos SAE claves
            // distintas sin un solo error. Un emisor viejo no manda la huella:
            // se acepta como antes, para no romper un despliegue mezclado.
            if let Some(expected) = k
                .extension
                .as_ref()
                .and_then(|m| m.get("session_key_digest"))
                .and_then(|v| v.as_str())
            {
                let actual = crate::southbound::orr::key_digest(&k.key_id.to_string(), &plaintext);
                if actual != expected {
                    self.flow.recv_corrupt(peer.as_str(), 1);
                    warn!(
                        %peer,
                        key_id = %k.key_id,
                        transport_key_id = %tk_id,
                        esperado = %expected,
                        obtenido = %actual,
                        "dkms.ext_keys: la clave de sesión no cuadra con su huella; \
                         la descarto y NO la acuso. Casi seguro la clave de transporte \
                         difiere entre los dos extremos: sin esto, los dos SAE de esta \
                         petición se habrían llevado claves DISTINTAS",
                    );
                    continue;
                }
            }

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

    /// ACK entrante por el plano ETSI-020 (mTLS): un peer nos confirma que
    /// recibió las claves con estos `key_ids`, y las movemos de `ack_pending`
    /// a `buffer_enc` vía `Generator::on_ack`. La identidad del emisor es la
    /// del **cert** (`peer`), no un campo del cuerpo — a diferencia del socket
    /// TCP plano heredado, cuyo `from` es autodeclarado (docs/SECURITY.md
    /// §Fase 4). Devuelve cuántos `key_ids` casaron. Ruta aditiva: hoy los
    /// ACKs salientes usan el socket; migrar la salida aquí y retirar el
    /// socket queda pendiente de verificación en testbed.
    pub fn handle_incoming_ack(&self, peer: &NodeId, key_ids: &[Etsi020KeyID]) -> usize {
        let Some(gen) = self.generator.as_ref() else {
            return 0;
        };
        key_ids
            .iter()
            .filter(|kid| gen.on_ack(peer.as_str(), &KeyId::new(kid.key_id.to_string())))
            .count()
    }

    // ─── Helpers internos ──────────────────────────────────────────────

    /// Registra la ejecución del peer y, si ha cambiado, olvida el material
    /// que compartíamos con la anterior.
    ///
    /// El reinicio de un DKMS lo dejaba sin recibir nada de sus peers hasta
    /// que un SAE pidiera claves y el rechazo delatara el desfase
    /// (`resync_transport_buffer_if_stale`). Sin demanda no había rechazo:
    /// medido el 2026-08-20, un nodo reiniciado emitía con normalidad y se
    /// quedaba con `dec=0 recv=0` contra todos sus peers indefinidamente,
    /// mientras sus `buffer_enc` seguían a tope de claves cuya mitad DEC ya
    /// no existía. El detector es su propio tráfico de relleno: un DKMS que
    /// arranca tiene el ENC vacío, así que emite enseguida, y cada
    /// `DKMS_BUFFER` lleva su encarnación.
    ///
    /// Se tiran las tres cosas que quedan colgando de la ejecución anterior:
    /// nuestro `buffer_enc` (su `buffer_dec` ya no las tiene), nuestro
    /// `buffer_dec` (su `buffer_enc` tampoco) y lo pendiente de ACK, que
    /// cuenta contra el tope de emisión y retrasaría el relleno un TTL
    /// entero.
    /// Cooldown entre wipes por peer disparados por cambio de `incarnation`.
    /// Un reinicio legítimo cambia la incarnation una vez; este margen impide
    /// que un peer que la cambie en cada mensaje (bug o abuso, viene en un
    /// header ORR sin autenticar) vacíe el buffer en bucle. Ver `note_at`.
    const INCARNATION_WIPE_COOLDOWN: Duration = Duration::from_secs(30);

    fn note_peer_incarnation(&self, peer: &str, incarnation: &str) {
        if let Some(previous) = self.peer_incarnations.note_at(
            peer,
            incarnation,
            std::time::Instant::now(),
            Self::INCARNATION_WIPE_COOLDOWN,
        ) {
            self.forget_material_of_previous_incarnation(peer, &previous, incarnation);
        }
    }

    fn forget_material_of_previous_incarnation(&self, peer: &str, previous: &str, current: &str) {
        let buf = self.pool.for_peer(peer);
        let enc = buf.enc_clear();
        let dec = buf.dec.len();
        buf.dec.clear();
        let pending = self
            .generator
            .as_ref()
            .map(|g| g.ack_pending.drop_peer(peer))
            .unwrap_or(0);
        warn!(
            peer,
            previous,
            current,
            enc_discarded = enc,
            dec_discarded = dec,
            ack_pending_discarded = pending,
            "el peer se ha reiniciado: olvido el material compartido con su ejecución anterior \
             para que el generador vuelva a llenarle el buffer",
        );
    }

    /// Si el peer rechazó el envío porque no reconoce la clave de transporte,
    /// nuestro `buffer_enc[peer]` está desincronizado del suyo: tira el
    /// nuestro para que el generador lo rehaga.
    ///
    /// Sin esto, un reinicio del peer lo dejaba inalcanzable **para siempre**.
    /// El generador solo repone `buffer_enc` cuando baja del tope, y estaba a
    /// tope de claves que el peer ya no tiene; cada petición SAE gastaba una
    /// y fallaba, y con 4096 en la recámara eso no se agota nunca en la
    /// práctica. Ver [`crate::state::pool::PeerBuffers::enc_clear`].
    fn resync_transport_buffer_if_stale(&self, e: &DkmsError) {
        let Some(peer) = peer_with_stale_transport_buffer(e) else {
            return;
        };
        let discarded = self.pool.for_peer(peer).enc_clear();
        warn!(
            peer,
            discarded,
            "el peer no reconoce mi clave de transporte (se habrá reiniciado): \
             descarto mi buffer_enc para que el generador lo rehaga",
        );
    }

    fn build_ext_keys_envelope(
        &self,
        master: &SaeId,
        peer_node: &NodeId,
        peer_saes: &[SaeId],
        session_keys: &[(Uuid, KeyId, Zeroizing<Vec<u8>>)],
        serve_pref: &[common::security::KeyGrade],
    ) -> Result<Etsi020ExtKeyContainer> {
        let peer_buffers = self.pool.for_peer(peer_node.as_str());
        let mut etsi_keys = Vec::with_capacity(session_keys.len());
        for (uuid, _kid, k_bytes) in session_keys {
            // Pop a transport key of the preferred grade (the request's
            // security level decides the order). A QKD-grade key never comes
            // from a PQC buffer and vice-versa.
            let tk = peer_buffers.enc_pop_pref(serve_pref).ok_or_else(|| {
                DkmsError::TransportBufferEmpty {
                    peer: peer_node.to_string(),
                }
            })?;
            if tk.bytes.len() < TRANSPORT_KEY_BYTES {
                return Err(DkmsError::Crypto(format!(
                    "transport key too short ({} bytes, need {})",
                    tk.bytes.len(),
                    TRANSPORT_KEY_BYTES
                )));
            }
            let ciphertext = wrap_session_key(tk.bytes.as_slice(), k_bytes.as_slice())
                .map_err(DkmsError::Crypto)?;
            let mut ext = serde_json::Map::new();
            ext.insert(
                "transport_key_id".to_owned(),
                Value::String(tk.id.to_string()),
            );
            // Huella de la clave de SESIÓN, para que el peer compruebe que la
            // ha desenvuelto bien antes de guardarla.
            //
            // Es la única comprobación de este camino. La clave va envuelta en
            // OTP con una clave de transporte, y si esa clave difiere entre los
            // dos extremos —épocas desincronizadas, un buffer que no se limpió—
            // el peer desenvuelve otra cosa, la guarda tan tranquilo, y los dos
            // SAE de la misma petición ETSI-014 acaban con claves DISTINTAS sin
            // un solo error por ningún lado. Es el mismo patrón que el
            // `key_digest` del camino del generador.
            //
            // Publicarla no filtra nada: esto viaja dentro de mTLS DKMS↔DKMS, y
            // la clave son 256 bits de entropía, así que la preimagen no es
            // atacable. Va ligada al `key_id` para que no valga en otra entrada.
            ext.insert(
                "session_key_digest".to_owned(),
                Value::String(crate::southbound::orr::key_digest(
                    &uuid.to_string(),
                    k_bytes.as_slice(),
                )),
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
        let msg_type = app.get(HDR_MSG_TYPE).map(String::as_str).unwrap_or("");

        if msg_type == MSG_TYPE_DKMS_BUFFER {
            return self.handle_orr_delivery_buffer(msg).await;
        }
        // Cualquier otra cosa: el ORR no debe transportar SAE-keys; ese
        // path va siempre por HTTP ETSI 020 DKMS↔DKMS (OTP con
        // buffer_enc). Si llega algo legacy con msg_type vacío,
        // lo descartamos con un WARN para no ensuciar la pending store.
        let key_id = app.get(HDR_KEY_ID).cloned().unwrap_or_default();
        static UNKNOWN_MSG_TYPE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let n = UNKNOWN_MSG_TYPE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if common::log_throttle::nth_is_loud(n) {
            warn!(
                msg_type,
                key_id,
                discarded = n + 1,
                "orr delivery con msg_type no reconocido — descartado; \
                 SAE-keys ya no viajan por ORR (van por HTTP ETSI 020)"
            );
        }
        Ok(())
    }

    /// Maneja un `DKMS_BUFFER` entrante: el peer DKMS_A está rellenando
    /// nuestro `buffer_dec[A]` con una clave fresca. Lo guardamos y
    /// disparamos el ACK por socket TCP plano (no por ORR ni QKC).
    async fn handle_orr_delivery_buffer(
        &self,
        msg: common::proto::orr::v1::DeliveredMessage,
    ) -> Result<()> {
        let app = &msg.app_header;
        let key_id_str = app
            .get(HDR_KEY_ID)
            .ok_or_else(|| {
                DkmsError::BadRequest(format!("orr buffer delivery missing {HDR_KEY_ID}"))
            })?
            .clone();
        let source_dkms = app
            .get(HDR_SAE_ORIGIN)
            .ok_or_else(|| {
                DkmsError::BadRequest(format!("orr buffer delivery missing {HDR_SAE_ORIGIN}"))
            })?
            .clone();
        let ack_endpoint = app.get(HDR_ACK_ENDPOINT).cloned();

        let bits = app
            .get(HDR_KEY_SIZE_BITS)
            .and_then(|v| v.parse::<u32>().ok());
        if let Some(bits) = bits {
            let expected = (bits as usize).div_ceil(8);
            if msg.payload.len() != expected {
                return Err(DkmsError::BadRequest(format!(
                    "orr buffer delivery {key_id_str}: payload {} bytes, header {bits} bits",
                    msg.payload.len(),
                )));
            }
        }

        // Abrir ANTES de tocar nada. El payload llega sellado por el DKMS
        // origen (`crate::e2e`): el tag cubre el material y la cabecera
        // entera, así que hasta aquí no nos fiamos ni de `incarnation` —
        // que borra buffers— ni de `ack_endpoint`. Un tag que no cuadra es
        // corrupción, alteración o un secreto divergente: se descarta sin
        // acusar recibo y el emisor la verá expirar. Una época que no
        // tenemos es que alguien reinició: se pide un acuerdo (con su
        // rate-limit) y mientras tanto lo que llegue se descarta igual.
        let plaintext = match self.e2e.open(&source_dkms, app, &key_id_str, &msg.payload) {
            Ok(pt) => pt,
            Err(crate::e2e::E2eError::UnknownEpoch { epoch, .. }) => {
                let n = self
                    .flow
                    .peer(&source_dkms)
                    .recv_no_epoch
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.e2e.request_agreement(&source_dkms);
                if n == 0 || n.is_multiple_of(1000) {
                    warn!(
                        source = %source_dkms,
                        epoch,
                        dropped = n + 1,
                        "e2e: clave sellada con una época que no tengo; pido acuerdo y descarto",
                    );
                }
                return Ok(());
            }
            Err(crate::e2e::E2eError::Replay(e)) => {
                // Los dos contadores van a `generator.state`; el log habla en
                // las potencias de dos — un secreto divergente es CADA clave.
                self.flow.recv_replayed(&source_dkms, 1);
                let n = self
                    .flow
                    .peer(&source_dkms)
                    .recv_replayed
                    .load(std::sync::atomic::Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n.saturating_sub(1)) {
                    warn!(source = %source_dkms, key_id = %key_id_str, error = %e,
                          replayed = n, "e2e: clave repetida, descartada");
                }
                return Ok(());
            }
            Err(e) => {
                self.flow.recv_corrupt(&source_dkms, 1);
                let n = self
                    .flow
                    .peer(&source_dkms)
                    .recv_corrupt
                    .load(std::sync::atomic::Ordering::Relaxed);
                if common::log_throttle::nth_is_loud(n.saturating_sub(1)) {
                    warn!(
                        source = %source_dkms,
                        key_id = %key_id_str,
                        error = %e,
                        corrupt = n,
                        "e2e: clave de transporte NO verificable: la descarto sin acusar recibo. \
                         Alterada en tránsito, cabecera manipulada o secreto e2e divergente",
                    );
                }
                return Ok(());
            }
        };

        // Ya autenticada la cabecera: ¿sigue siendo la misma ejecución del peer?
        if let Some(inc) = app.get(HDR_INCARNATION) {
            self.note_peer_incarnation(&source_dkms, inc);
        }

        let key_id = KeyId::new(&key_id_str);
        let buf = self.pool.for_peer(&source_dkms);
        let key = TransportKey {
            id: key_id.clone(),
            bytes: zeroize::Zeroizing::new(plaintext),
        };
        // `try_push` ahora es soft-hint en capacidad (nunca rechaza).
        // El cap antiguo provocaba un deadlock cuando el dec se llenaba:
        // se dropeaba la key, no se enviaba ACK, el `ack_pending` del
        // source expiraba, y como su métrica de fill incluía
        // `enc + ack_pending` quedaba atascado en `Saturated` con
        // SDN-rate=0. Ahora siempre push + siempre ACK; el RAM se
        // acota por la rate del SDN y los SAEs drenan vía pop. Si algún día
        // vuelve a rechazar, que se vea en `generator.state`.
        if buf.dec.try_push(key).is_err() {
            self.flow
                .peer(&source_dkms)
                .dec_dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }

        // Contadores del camino de claves. `recv` es la mitad del
        // diagnóstico que faltaba: sin él, "no le llegan mis claves" y "no
        // me llegan sus ACK" se ven exactamente igual desde el emisor.
        let n_recv = self
            .flow
            .peer(&source_dkms)
            .recv
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if n_recv == 0 {
            info!(
                source = %source_dkms,
                ack_endpoint = ack_endpoint.as_deref().unwrap_or("<none>"),
                "dkms: primera clave recibida de este peer por ORR",
            );
        }

        // ACK siempre (TCP plano, no ORR), batched por
        // `BatchedAckClient` para amortizar el coste de open/close
        // bajo carga sostenida.
        if let Some(ep) = ack_endpoint {
            // Un cambio de `ack_endpoint` a mitad de despliegue (peer
            // recreado con otra IP) explicaría ACKs que dejan de llegar;
            // se loguea una vez por valor, no por clave.
            if self.flow.note_endpoint(&source_dkms, &ep) {
                info!(
                    source = %source_dkms,
                    ack_endpoint = %ep,
                    "dkms: acusaré recibo a este peer en esta dirección",
                );
            }
            if let Some(client) = self.ack_client.clone() {
                client.enqueue(&source_dkms, ep, key_id_str.clone()).await;
            } else {
                self.flow.ack_no_endpoint(&source_dkms, 1);
                debug!(
                    source = %source_dkms,
                    key_id = %key_id_str,
                    "orr buffer delivery: no ack_client configured, skipping ACK",
                );
            }
        } else {
            self.flow.ack_no_endpoint(&source_dkms, 1);
            debug!(
                source = %source_dkms,
                key_id = %key_id_str,
                "orr buffer delivery: no ack_endpoint in header, peer won't see ACK",
            );
        }

        debug!(
            source = %source_dkms,
            key_id = %key_id_str,
            buffer_dec_len = buf.dec.len(),
            "orr buffer delivery → buffer_dec[source]",
        );
        Ok(())
    }
}

/// Peer cuyo rechazo delata que nuestro `buffer_enc` para él está obsoleto.
///
/// El cuerpo es el `DkmsError::TransportKeyMissing` que serializó el peer;
/// casamos por su parte estable. Si cambias ese texto en `error.rs`, cambia
/// también este literal — el test de abajo lo ata a la variante real.
fn peer_with_stale_transport_buffer(e: &DkmsError) -> Option<&str> {
    match e {
        DkmsError::PeerRejected { peer, body, .. } if body.contains("not in buffer_dec") => {
            Some(peer.as_str())
        }
        _ => None,
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

/// Cifra `k` con **OTP puro** (XOR) usando los primeros `k.len()` bytes
/// de `transport`. La clave QKD-distribuida actúa como one-time-pad:
/// se usa una sola vez y se quema (el llamador es responsable de
/// `take_by_id` / `pop_oldest` en el buffer).
///
/// La integridad la garantiza la capa TLS (mTLS DKMS↔DKMS); por eso
/// OTP-only es suficiente — no añadimos MAC.
fn wrap_session_key(transport: &[u8], k: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if transport.len() < k.len() {
        return Err(format!(
            "transport key too short for OTP ({} bytes, session key {} bytes)",
            transport.len(),
            k.len()
        ));
    }
    let mut out = Vec::with_capacity(k.len());
    for (kb, tb) in k.iter().zip(transport.iter()) {
        out.push(kb ^ tb);
    }
    Ok(out)
}

fn unwrap_session_key(transport: &[u8], wire: &[u8]) -> std::result::Result<Vec<u8>, String> {
    if transport.len() < wire.len() {
        return Err(format!(
            "transport key too short for OTP ({} bytes, ciphertext {} bytes)",
            transport.len(),
            wire.len()
        ));
    }
    let mut out = Vec::with_capacity(wire.len());
    for (wb, tb) in wire.iter().zip(transport.iter()) {
        out.push(wb ^ tb);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bindings(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(s, n)| (s.to_string(), n.to_string()))
            .collect()
    }

    /// La huella de la clave de sesión: lo que impide que dos SAE de la misma
    /// petición se lleven claves distintas cuando la clave de transporte
    /// diverge entre los dos extremos.
    #[test]
    fn session_key_digest_catches_a_diverged_transport_key() {
        use crate::southbound::orr::key_digest;
        let key_id = uuid::Uuid::new_v4().to_string();
        let session_key = vec![0xA5u8; TRANSPORT_KEY_BYTES];
        let transport_ok = vec![0x11u8; TRANSPORT_KEY_BYTES];
        let wire = wrap_session_key(&transport_ok, &session_key).unwrap();
        let expected = key_digest(&key_id, &session_key);

        // Con la MISMA clave de transporte, la huella cuadra.
        let pt = unwrap_session_key(&transport_ok, &wire).unwrap();
        assert_eq!(key_digest(&key_id, &pt), expected);

        // Con una clave de transporte que difiere en UN bit, el desenvuelto da
        // otra cosa — y con OTP nada más abajo lo nota. La huella sí.
        let mut transport_bad = transport_ok.clone();
        transport_bad[0] ^= 0x01;
        let pt_bad = unwrap_session_key(&transport_bad, &wire).unwrap();
        assert_ne!(pt_bad, session_key);
        assert_ne!(key_digest(&key_id, &pt_bad), expected);
    }

    /// La huella va ligada al `key_id`, así que una clave buena no vale para
    /// otra entrada.
    #[test]
    fn session_key_digest_is_bound_to_its_key_id() {
        use crate::southbound::orr::key_digest;
        let session_key = vec![0x5Au8; TRANSPORT_KEY_BYTES];
        let a = key_digest(&uuid::Uuid::new_v4().to_string(), &session_key);
        let b = key_digest(&uuid::Uuid::new_v4().to_string(), &session_key);
        assert_ne!(a, b);
    }

    #[test]
    fn sae_authorization_only_accepts_served_saes() {
        let map = bindings(&[
            ("sae-local-1", "dkms-a"),
            ("sae-local-2", "dkms-a"),
            ("sae-elsewhere", "dkms-b"),
        ]);

        // Un SAE que este DKMS declara servir pasa.
        assert!(sae_served_locally(
            &map,
            "dkms-a",
            &SaeId::new("sae-local-1")
        ));
        // Uno que reside en otro DKMS, no — aunque su cert sea válido.
        assert!(!sae_served_locally(
            &map,
            "dkms-a",
            &SaeId::new("sae-elsewhere")
        ));
        // Uno desconocido, tampoco.
        assert!(!sae_served_locally(
            &map,
            "dkms-a",
            &SaeId::new("sae-ghost")
        ));
        // Con el mapa vacío no se sirve a nadie (fail-closed).
        assert!(!sae_served_locally(
            &HashMap::new(),
            "dkms-a",
            &SaeId::new("sae-local-1")
        ));
    }

    #[test]
    fn otp_wrap_unwrap_roundtrip() {
        let mut transport = vec![0u8; TRANSPORT_KEY_BYTES];
        OsRng.fill_bytes(&mut transport);
        let k = b"this is a session key K".to_vec();
        let wire = wrap_session_key(&transport, &k).unwrap();
        assert_eq!(
            wire.len(),
            k.len(),
            "OTP ciphertext has same length as plaintext"
        );
        let pt = unwrap_session_key(&transport, &wire).unwrap();
        assert_eq!(pt, k);
    }

    #[test]
    fn otp_rejects_short_transport_key() {
        let transport = vec![0u8; 8];
        let k = vec![0u8; 32];
        assert!(wrap_session_key(&transport, &k).is_err());
    }

    /// El literal que buscamos tiene que seguir saliendo de la variante real:
    /// si alguien reescribe el `#[error]` de `TransportKeyMissing`, este test
    /// cae antes de que el laboratorio descubra que el enlace ya no se cura.
    #[test]
    fn a_peer_that_lost_our_transport_keys_is_detected() {
        let real_body = DkmsError::TransportKeyMissing {
            peer: "dkms-1".into(),
            key_id: "abc".into(),
        }
        .to_string();
        let rejected = DkmsError::PeerRejected {
            peer: "dkms-2".into(),
            status: 503,
            body: real_body,
        };
        assert_eq!(
            peer_with_stale_transport_buffer(&rejected),
            Some("dkms-2"),
            "el rechazo identifica al peer cuyo buffer_enc hay que tirar",
        );

        // Otros rechazos no deben provocar que tiremos el buffer.
        let otro = DkmsError::PeerRejected {
            peer: "dkms-2".into(),
            status: 429,
            body: "rate-limited".into(),
        };
        assert_eq!(peer_with_stale_transport_buffer(&otro), None);
        assert_eq!(
            peer_with_stale_transport_buffer(&DkmsError::PeerAckTimeout {
                peer: "dkms-2".into()
            }),
            None,
        );
    }

    /// Los límites de `/status` tienen que rechazar, no recortar en silencio.
    #[test]
    fn request_limits_reject_what_status_says_is_too_much() {
        let ok = Etsi014KeyRequest {
            number: 1,
            size: 256,
            ..Default::default()
        };
        assert!(check_request_limits(&ok, 1).is_ok());
        assert!(
            check_request_limits(
                &Etsi014KeyRequest {
                    number: MAX_KEY_PER_REQUEST,
                    ..ok.clone()
                },
                1
            )
            .is_ok(),
            "el máximo anunciado es válido, no uno menos"
        );

        // number por encima de max_key_per_request: antes devolvía las 65.
        assert!(check_request_limits(
            &Etsi014KeyRequest {
                number: MAX_KEY_PER_REQUEST + 1,
                ..ok.clone()
            },
            1
        )
        .is_err());

        // size no múltiplo de 8: antes redondeaba a 8 bits y devolvía 200.
        assert!(check_request_limits(
            &Etsi014KeyRequest {
                size: 7,
                ..ok.clone()
            },
            1
        )
        .is_err());

        // fuera del rango anunciado: antes el extremo alto acababa en 500.
        for size in [MIN_KEY_SIZE_BITS - 8, MAX_KEY_SIZE_BITS + 8, 100_000] {
            assert!(
                check_request_limits(&Etsi014KeyRequest { size, ..ok.clone() }, 1).is_err(),
                "size {size} debería rechazarse"
            );
        }

        // Y el recuento de destinos, que se mide tras deduplicar.
        assert!(check_request_limits(&ok, MAX_SAE_ID_COUNT).is_ok());
        assert!(check_request_limits(&ok, MAX_SAE_ID_COUNT + 1).is_err());
    }

    #[test]
    fn enc_clear_empties_both_grades_and_counts() {
        use common::security::KeyGrade;

        use crate::state::buffer::TransportKey;
        let pool = BufferPool::new(8);
        let buf = pool.for_peer("dkms-2");
        for (i, g) in [KeyGrade::Qkd, KeyGrade::Pqc].into_iter().enumerate() {
            buf.enc(g)
                .try_push(TransportKey::new(
                    KeyId::new(format!("k{i}")),
                    vec![0xAB; 32],
                ))
                .expect("hay hueco");
        }
        assert_eq!(buf.enc_len(), 2);
        assert_eq!(buf.enc_clear(), 2, "devuelve cuántas tiró");
        assert_eq!(buf.enc_len(), 0, "los dos grados quedan vacíos");
    }
}
