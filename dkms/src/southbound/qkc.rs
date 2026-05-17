//! Cliente gRPC del DKMS contra QKC.
//!
//! Hoy QKC expone `Reserve` / `Release` para la coordinación pero el
//! material de la clave de transporte real viaja por el plano binario
//! `qkc/transport` (en flujo durante este sprint).  Para que el DKMS pueda
//! avanzar sin esperar a QKC, este cliente define un método de alto nivel
//! [`QkcClient::reserve_keys`] que devuelve los `KeyId` reservados.  Cuando
//! QKC estabilice el canal de entrega de bytes (gRPC streaming, *get* por
//! id, lo que sea), añadimos aquí el método correspondiente.
//!
//! Mientras tanto, el alimentador del buffer ENC trabaja en modo
//! *placeholder* (ver [`crate::service`]): genera bytes localmente y los
//! asocia al `KeyId` devuelto por `Reserve` para mantener funcional el
//! plano DKMS↔DKMS.  Esa lógica vive en `service`, **no** aquí, para que el
//! día que QKC esté listo simplemente cambie el alimentador.

use std::time::Duration;

use anyhow::anyhow;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::debug;

use common::ids::{KeyId, NodeId};
use common::proto::{
    common::v1::NodeId as ProtoNodeId,
    qkc::v1::{qkc_control_client::QkcControlClient, ReleaseRequest, ReserveRequest},
};

use crate::{
    config::SouthboundCfg,
    error::{DkmsError, Result},
};

#[derive(Clone)]
pub struct QkcClient {
    channel: Channel,
    rpc_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct Reservation {
    pub reservation_id: String,
    pub key_ids: Vec<KeyId>,
    pub expires_at_unix_ms: i64,
}

impl QkcClient {
    pub async fn connect(cfg: &SouthboundCfg, tls: Option<ClientTlsConfig>) -> Result<Self> {
        let mut endpoint = Endpoint::from_shared(cfg.qkc_endpoint.clone())
            .map_err(|e| DkmsError::QkcUnreachable(anyhow!(e)))?
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            .timeout(Duration::from_millis(cfg.rpc_timeout_ms));

        if let Some(t) = tls {
            endpoint = endpoint
                .tls_config(t)
                .map_err(|e| DkmsError::QkcUnreachable(anyhow!(e)))?;
        }

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| DkmsError::QkcUnreachable(anyhow!(e)))?;

        debug!(endpoint = %cfg.qkc_endpoint, "qkc client connected");
        Ok(Self {
            channel,
            rpc_timeout: Duration::from_millis(cfg.rpc_timeout_ms),
        })
    }

    fn client(&self) -> QkcControlClient<Channel> {
        QkcControlClient::new(self.channel.clone())
    }

    /// Reserva `count` claves entre `src` y `dst` con un `reservation_id`
    /// idempotente, devolviendo los `KeyId`s.
    pub async fn reserve_keys(
        &self,
        src: &NodeId,
        dst: &NodeId,
        count: u32,
        size_bits: u32,
        reservation_id: impl Into<String>,
        ttl_ms: u32,
    ) -> Result<Reservation> {
        let mut c = self.client();
        let reservation_id = reservation_id.into();
        let req = tonic::Request::new(ReserveRequest {
            src: Some(ProtoNodeId {
                value: src.to_string(),
            }),
            dst: Some(ProtoNodeId {
                value: dst.to_string(),
            }),
            count,
            size_bits,
            reservation_id: reservation_id.clone(),
            ttl_ms,
        });
        let resp = tokio::time::timeout(self.rpc_timeout, c.reserve(req))
            .await
            .map_err(|_| DkmsError::QkcUnreachable(anyhow!("reserve timeout")))?
            .map_err(|s| DkmsError::QkcUnreachable(anyhow!(s)))?
            .into_inner();
        Ok(Reservation {
            reservation_id: resp.reservation_id,
            key_ids: resp
                .key_ids
                .into_iter()
                .map(|k| KeyId::new(k.value))
                .collect(),
            expires_at_unix_ms: resp.expires_at_unix_ms,
        })
    }

    pub async fn release(&self, reservation_id: impl Into<String>) -> Result<()> {
        let mut c = self.client();
        let req = tonic::Request::new(ReleaseRequest {
            reservation_id: reservation_id.into(),
        });
        tokio::time::timeout(self.rpc_timeout, c.release(req))
            .await
            .map_err(|_| DkmsError::QkcUnreachable(anyhow!("release timeout")))?
            .map_err(|s| DkmsError::QkcUnreachable(anyhow!(s)))?;
        Ok(())
    }
}
