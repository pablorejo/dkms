//! Servicio ORR: pega gRPC (DKMS side) con el wire TCP del QKC local
//! y orquesta los cuatro modos de routing (passthrough / PQC E2E /
//! cebolla SDN / cebolla truncada).
//!
//! Mantiene:
//!   * `cfg`        — configuración inmutable.
//!   * `identity`   — par ML-KEM persistente del ORR (`OrrIdentity`).
//!   * `peers`      — directorio `orr_id → (qkc_id, pubkey)`.
//!   * `circuits`   — tabla de circuitos (sólo modos PQC/onion, opcional).
//!   * `qkc_link`   — conexión TCP persistente al QKC co-localizado.
//!   * `deliveries` — broadcast `tokio::sync::broadcast` de los
//!                     `DeliveredMessage` que se entregan localmente.
//!
//! El pump de entrada (frames `FRAME_LOCAL_DELIVER` del QKC) llama a
//! [`OrrService::handle_incoming`], que:
//!   * Si la cabecera no es de tipo `ORR`, descarta.
//!   * Si `pqc_layer == true`, pela una capa onion con la sk local y
//!     o reenvía (`InnerLayer::Forward`) o entrega
//!     (`InnerLayer::Deliver`).
//!   * En caso contrario (passthrough), publica el payload tal cual.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use common::metrics::Metrics;
use common::proto::common::v1::NodeId;
use common::proto::orr::v1::DeliveredMessage;
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};
use wire::{Frame, FRAME_LOCAL_SEND};

use crate::{
    config::OrrConfig,
    error::{OrrError, Result},
    header::{OrrHeader, HEADER_TYPE},
    identity::OrrIdentity,
    onion::{self, InnerLayer, PathHop},
    peers::PeerRegistry,
    qkc_link::QkcLink,
    relay::CircuitTable,
};

/// Resultado de un envío. Se exporta porque `grpc_server` lo traduce a
/// `SendMessageResponse`.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub status:         &'static str,
    pub final_dest_orr: String,
    pub next_hop_qkc:   u32,
    pub remaining_hops: i32,
    pub pqc_layer:      bool,
}

#[derive(Clone)]
pub struct OrrService {
    pub cfg:       Arc<OrrConfig>,
    pub identity:  Arc<OrrIdentity>,
    pub circuits:  Arc<CircuitTable>,
    pub peers:     Arc<PeerRegistry>,
    pub metrics:   Metrics,
    pub qkc_link:  QkcLink,
    deliveries_tx: broadcast::Sender<DeliveredMessage>,
}

impl OrrService {
    pub async fn new(cfg: OrrConfig, metrics: Metrics) -> Result<Self> {
        // Identidad ML-KEM (long-term, regenerada cada vez que arranca
        // el proceso — TODO: persistir en disco si queremos pubkeys
        // estables entre reinicios).
        let identity = Arc::new(OrrIdentity::generate(
            cfg.orr_id.clone(),
            &cfg.default_pqc_suite,
        )?);
        info!(
            orr_id = %cfg.orr_id,
            suite = %identity.suite,
            pubkey_len = identity.public_key.len(),
            "orr.identity generated",
        );

        // Decodificar pubkeys de peers desde base64.
        let mut peer_pubkeys: HashMap<String, Vec<u8>> = HashMap::new();
        for (orr_id, b64) in &cfg.peer_pubkeys {
            let bytes = BASE64.decode(b64).map_err(|e| {
                OrrError::Relay(format!("peer {orr_id} pubkey base64 inválido: {e}"))
            })?;
            peer_pubkeys.insert(orr_id.clone(), bytes);
        }

        let cfg = Arc::new(cfg);
        let peers = Arc::new(PeerRegistry::with_pubkeys(
            cfg.peers.clone(),
            peer_pubkeys,
            cfg.orr_id.clone(),
            cfg.qkc_id,
        ));

        let (deliveries_tx, _) = broadcast::channel(cfg.deliver_queue_capacity.max(1));
        let (frames_tx, mut frames_rx) =
            mpsc::channel::<Frame>(cfg.deliver_queue_capacity.max(1));
        let qkc_link = QkcLink::spawn(cfg.qkc_local_addr.clone(), frames_tx);

        let svc = OrrService {
            cfg,
            identity,
            circuits: Arc::new(CircuitTable::new()),
            peers,
            metrics,
            qkc_link,
            deliveries_tx,
        };

        // Pump entrante: cada frame `FRAME_LOCAL_DELIVER` del QKC pasa
        // por `handle_incoming`, que decide pelar capa onion vs.
        // entregar directo.
        let pump = svc.clone();
        tokio::spawn(async move {
            while let Some(frame) = frames_rx.recv().await {
                if let Err(e) = pump.handle_incoming(frame).await {
                    warn!(error = %e, "orr.incoming handle_failed");
                }
            }
            warn!("orr.incoming pump stopped");
        });

        info!(
            orr_id = %svc.cfg.orr_id,
            qkc_id = svc.cfg.qkc_id,
            qkc_addr = %svc.cfg.qkc_local_addr,
            "orr service initialized",
        );
        Ok(svc)
    }

    /// Suscribirse al stream de mensajes entrantes locales.
    pub fn subscribe_deliveries(&self) -> broadcast::Receiver<DeliveredMessage> {
        self.deliveries_tx.subscribe()
    }

    /// Public key ML-KEM de este ORR (para que se la pidan los peers).
    pub fn public_key(&self) -> &[u8] {
        &self.identity.public_key
    }

    // ─── send_message: dispatch por max_hops ───────────────────────────

    pub async fn send_message(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        max_hops: i32,
        app_header: BTreeMap<String, String>,
    ) -> Result<SendOutcome> {
        // Entrega trivial a sí mismo (cualquier modo).
        if dest_orr == self.cfg.orr_id {
            self.broadcast_self(payload, app_header);
            return Ok(SendOutcome {
                status:         "delivered_local",
                final_dest_orr: dest_orr.into(),
                next_hop_qkc:   self.cfg.qkc_id,
                remaining_hops: 0,
                pqc_layer:      false,
            });
        }

        match max_hops {
            0 => self.send_passthrough(dest_orr, payload, app_header).await,
            1 => self.send_onion_e2e(dest_orr, payload, app_header).await,
            -1 => self.send_onion_path(dest_orr, payload, app_header, None).await,
            n if n >= 2 => {
                self.send_onion_path(dest_orr, payload, app_header, Some(n as usize)).await
            }
            n => Err(OrrError::InvalidPath(format!("max_hops inválido: {n}"))),
        }
    }

    /// `max_hops = 0`: el ORR no añade nada. Empuja el payload al QKC
    /// con `dest_final = qkc(dest)`. La confidencialidad y routing
    /// son del QKC.
    async fn send_passthrough(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
    ) -> Result<SendOutcome> {
        let dest_qkc = self.peers.qkc_id(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!("ORR destino {dest_orr} no resoluble (peers config)"))
        })?;
        let mut header = OrrHeader::new(&self.cfg.orr_id, dest_orr, 0);
        if let Some(fid) = app_header.get("flow_id").cloned() {
            header.flow_id = Some(fid);
        }
        header.app_header = app_header;
        let header_mp = header.encode()?;
        let frame = Frame {
            kind:          FRAME_LOCAL_SEND,
            sender_id:     self.cfg.qkc_id,
            receiver_id:   self.cfg.qkc_id,
            dest_final:    dest_qkc,
            key_size_bits: 0,
            key_ids:       Vec::new(),
            header_mp,
            payload,
        };
        self.qkc_link.send(frame).await?;
        debug!(dest = %dest_orr, dest_qkc, "orr.send passthrough");
        Ok(SendOutcome {
            status:         "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc:   dest_qkc,
            remaining_hops: 0,
            pqc_layer:      false,
        })
    }

    /// `max_hops = 1`: una sola capa onion contra el ORR destino. El
    /// frame viaja por el QKC substrate (que sigue cifrando hop-by-hop
    /// con OTP de QKD); el ORR destino pela la capa PQC antes de
    /// entregar.
    async fn send_onion_e2e(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
    ) -> Result<SendOutcome> {
        let dest_qkc = self.peers.qkc_id(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!("ORR destino {dest_orr} no resoluble"))
        })?;
        let dest_pk = self.peers.public_key(dest_orr).ok_or_else(|| {
            OrrError::Relay(format!("ORR destino {dest_orr} sin pubkey ML-KEM"))
        })?;
        let path = vec![PathHop {
            orr_id:     dest_orr.into(),
            qkc_id:     dest_qkc,
            public_key: dest_pk,
        }];
        let onion_bytes = onion::build_onion(&*self.identity.kem, &path, payload)?;
        self.send_onion_frame(dest_orr, dest_qkc, onion_bytes, 0, app_header)
            .await?;
        debug!(dest = %dest_orr, dest_qkc, "orr.send pqc_e2e");
        Ok(SendOutcome {
            status:         "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc:   dest_qkc,
            remaining_hops: 0,
            pqc_layer:      true,
        })
    }

    /// `max_hops = -1` o `>=2`: cebolla multi-capa. El path viene del
    /// hint `app_header["orr_path"]` (CSV de orr_ids) mientras la SDN
    /// no esté cableada en Rust. Cuando llegue el cliente SDN, esto
    /// será una llamada a `SdnControl::ComputePath`.
    ///
    /// `cap = Some(N)` trunca el path a `N` ORRs (modo `max_hops >= 2`).
    /// `cap = None` toma el path completo (`max_hops = -1`).
    async fn send_onion_path(
        &self,
        dest_orr: &str,
        payload: Vec<u8>,
        app_header: BTreeMap<String, String>,
        cap: Option<usize>,
    ) -> Result<SendOutcome> {
        let hint = app_header.get("orr_path").cloned().ok_or_else(|| {
            OrrError::InvalidPath(
                "max_hops != 0,1 requiere `orr_path` en app_header (la SDN aún no está cableada)".into(),
            )
        })?;
        let mut full_path: Vec<String> = hint
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty() && s != &self.cfg.orr_id)
            .collect();
        if full_path.is_empty() {
            return Err(OrrError::InvalidPath("orr_path vacío tras filtrar self".into()));
        }
        // Asegurar que el destino sea el último elemento.
        if full_path.last().map(|s| s.as_str()) != Some(dest_orr) {
            full_path.push(dest_orr.to_string());
        }
        // Truncar si hay cap.
        if let Some(c) = cap {
            if c == 0 {
                return Err(OrrError::InvalidPath("cap == 0".into()));
            }
            if full_path.len() > c {
                full_path.truncate(c);
            }
        }

        // Resolver cada hop (qkc_id + pubkey).
        let mut hops = Vec::with_capacity(full_path.len());
        for orr_id in &full_path {
            let qkc_id = self.peers.qkc_id(orr_id).ok_or_else(|| {
                OrrError::Relay(format!("hop {orr_id} sin qkc_id en peers config"))
            })?;
            let pk = self.peers.public_key(orr_id).ok_or_else(|| {
                OrrError::Relay(format!("hop {orr_id} sin pubkey ML-KEM"))
            })?;
            hops.push(PathHop {
                orr_id:     orr_id.clone(),
                qkc_id,
                public_key: pk,
            });
        }
        let first = hops[0].clone();
        let onion_bytes = onion::build_onion(&*self.identity.kem, &hops, payload)?;
        let remaining = (hops.len() as i32) - 1;
        self.send_onion_frame(dest_orr, first.qkc_id, onion_bytes, remaining, app_header)
            .await?;
        debug!(
            dest = %dest_orr,
            first_hop = %first.orr_id,
            hops = hops.len(),
            "orr.send onion_path",
        );
        Ok(SendOutcome {
            status:         "sent",
            final_dest_orr: dest_orr.into(),
            next_hop_qkc:   first.qkc_id,
            remaining_hops: remaining,
            pqc_layer:      true,
        })
    }

    /// Mete un onion bundle dentro de un `FRAME_LOCAL_SEND` con
    /// `pqc_layer = true` y lo encola al QKC local.
    async fn send_onion_frame(
        &self,
        final_dest_orr: &str,
        next_qkc: u32,
        onion_bytes: Vec<u8>,
        remaining_hops: i32,
        app_header: BTreeMap<String, String>,
    ) -> Result<()> {
        let mut header = OrrHeader::new(&self.cfg.orr_id, final_dest_orr, remaining_hops);
        header.pqc_layer = true;
        header.pqc_destination_orr = Some(final_dest_orr.to_string());
        // En modos PQC no propagamos `flow_id` al top-level: el QKC no
        // debe rerutear por flow-table cuando el ORR fija el extremo.
        header.flow_id = None;
        header.app_header = app_header;
        let header_mp = header.encode()?;
        let frame = Frame {
            kind:          FRAME_LOCAL_SEND,
            sender_id:     self.cfg.qkc_id,
            receiver_id:   self.cfg.qkc_id,
            dest_final:    next_qkc,
            key_size_bits: 0,
            key_ids:       Vec::new(),
            header_mp,
            payload:       onion_bytes,
        };
        self.qkc_link.send(frame).await?;
        Ok(())
    }

    // ─── incoming: pelar onion vs. entregar passthrough ────────────────

    async fn handle_incoming(&self, frame: Frame) -> Result<()> {
        let header = if frame.header_mp.is_empty() {
            OrrHeader::default()
        } else {
            OrrHeader::decode(&frame.header_mp)?
        };

        // Filtra frames con type != ORR (otros protocolos sobre el
        // mismo QKC). No es un error, sólo no nos compete.
        if !header.kind.is_empty() && header.kind != HEADER_TYPE {
            debug!(kind = %header.kind, "orr.incoming ignore non-ORR");
            return Ok(());
        }

        if header.pqc_layer {
            self.handle_onion_in(&header, frame.payload).await
        } else {
            self.broadcast_delivery(&header, frame.payload);
            Ok(())
        }
    }

    async fn handle_onion_in(&self, header: &OrrHeader, payload: Vec<u8>) -> Result<()> {
        let layer = match onion::peel(&*self.identity.kem, &self.identity.secret_key, &payload) {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, from = %header.from, "orr.onion peel_failed");
                return Err(e);
            }
        };
        match layer {
            InnerLayer::Deliver { payload } => {
                debug!(from = %header.from, to = %header.to, "orr.onion deliver");
                self.broadcast_delivery(header, payload);
                Ok(())
            }
            InnerLayer::Forward { next_orr_id, next_qkc_id, inner } => {
                if next_orr_id == self.cfg.orr_id {
                    return Err(OrrError::Relay("onion forward loop to self".into()));
                }
                debug!(
                    next_orr = %next_orr_id,
                    next_qkc = next_qkc_id,
                    "orr.onion forward",
                );
                self.forward_onion(header, next_qkc_id, inner).await
            }
        }
    }

    async fn forward_onion(
        &self,
        prev_header: &OrrHeader,
        next_qkc_id: u32,
        inner: Vec<u8>,
    ) -> Result<()> {
        let remaining_after = (prev_header.max_hops - 1).max(0);
        let mut new_header = OrrHeader::new(&self.cfg.orr_id, &prev_header.to, remaining_after);
        new_header.pqc_layer = true;
        new_header.pqc_destination_orr = prev_header.pqc_destination_orr.clone();
        new_header.app_header = prev_header.app_header.clone();
        new_header.flow_id = None;
        let header_mp = new_header.encode()?;
        let out = Frame {
            kind:          FRAME_LOCAL_SEND,
            sender_id:     self.cfg.qkc_id,
            receiver_id:   self.cfg.qkc_id,
            dest_final:    next_qkc_id,
            key_size_bits: 0,
            key_ids:       Vec::new(),
            header_mp,
            payload:       inner,
        };
        self.qkc_link.send(out).await
    }

    // ─── delivery helpers ──────────────────────────────────────────────

    fn broadcast_self(&self, payload: Vec<u8>, app_header: BTreeMap<String, String>) {
        let _ = self.deliveries_tx.send(DeliveredMessage {
            origin: Some(NodeId { value: self.cfg.orr_id.clone() }),
            destination: Some(NodeId { value: self.cfg.orr_id.clone() }),
            payload,
            app_header: app_header.into_iter().collect(),
            received_at_unix_ms: now_unix_ms(),
            pqc_decapsulated: false,
        });
    }

    fn broadcast_delivery(&self, header: &OrrHeader, payload: Vec<u8>) {
        let dest = if header.to.is_empty() {
            self.cfg.orr_id.clone()
        } else {
            header.to.clone()
        };
        let origin = if header.from.is_empty() {
            None
        } else {
            Some(header.from.clone())
        };
        let msg = DeliveredMessage {
            origin: origin.map(|v| NodeId { value: v }),
            destination: Some(NodeId { value: dest }),
            payload,
            app_header: header.app_header.clone().into_iter().collect(),
            received_at_unix_ms: now_unix_ms(),
            pqc_decapsulated: header.pqc_layer,
        };
        if self.deliveries_tx.send(msg).is_err() {
            debug!("orr.deliver no_subscribers");
        }
    }
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
