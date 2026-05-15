//! Cliente gRPC del ORR contra la SDN.
//!
//! Mínimo viable: sólo expone `get_orr_path(src, dst) → Vec<orr_id>`,
//! que es lo que el ORR necesita para armar onions multi-hop cuando
//! el caller (DKMS) no le manda `orr_path` en el `app_header`.
//!
//! La cache de paths vive en [`crate::service::OrrService`] — aquí
//! sólo nos ocupamos del transporte.
//!
//! Conexión opcional: si la URL es vacía / inalcanzable al arrancar,
//! `connect_opt` devuelve `None` y el ORR sigue funcionando para los
//! modos 0/1 (que no necesitan path) y para los modos que sí reciben
//! `orr_path` por header.

use std::time::Duration;

use tonic::transport::{Channel, Endpoint};
use tracing::debug;

use common::proto::sdn::v1::{
    sdn_control_client::SdnControlClient, GetOrrPathRequest, GetOrrPathResponse,
    StreamTopologyRequest, TopologyEvent,
};
use tonic::Streaming;

use crate::error::{OrrError, Result};

#[derive(Clone)]
pub struct SdnClient {
    channel:     Channel,
    rpc_timeout: Duration,
}

impl SdnClient {
    /// Intenta conectar. URL vacía o "http://127.0.0.1:1" (placeholder)
    /// devuelve `Ok(None)` para no quemar tiempo en endpoints inválidos.
    pub async fn connect_opt(url: &str) -> Result<Option<Self>> {
        if url.is_empty() {
            return Ok(None);
        }
        let endpoint = Endpoint::from_shared(url.to_string())
            .map_err(|e| OrrError::Relay(format!("sdn endpoint inválido: {e}")))?
            .connect_timeout(Duration::from_millis(1500))
            .timeout(Duration::from_millis(3000));
        match endpoint.connect().await {
            Ok(channel) => {
                debug!(endpoint = url, "orr→sdn client connected");
                Ok(Some(Self {
                    channel,
                    rpc_timeout: Duration::from_millis(3000),
                }))
            }
            Err(e) => {
                debug!(endpoint = url, error = %e, "orr→sdn connect failed");
                Ok(None)
            }
        }
    }

    /// Abre el stream `StreamTopology`. Devuelve un `Streaming` que
    /// el caller drena en un task de fondo. No TTL/timeout — el
    /// stream se mantiene vivo hasta que falle, en cuyo caso el
    /// caller hace reconnect con backoff.
    pub async fn stream_topology(&self) -> Result<Streaming<TopologyEvent>> {
        let mut c = SdnControlClient::new(self.channel.clone());
        let req = tonic::Request::new(StreamTopologyRequest { since_version: 0 });
        let stream = c
            .stream_topology(req)
            .await
            .map_err(|s| OrrError::Relay(format!("sdn stream_topology: {s}")))?
            .into_inner();
        Ok(stream)
    }

    pub async fn get_orr_path(&self, src_orr: &str, dst_orr: &str) -> Result<Vec<String>> {
        let mut c = SdnControlClient::new(self.channel.clone());
        let req = tonic::Request::new(GetOrrPathRequest {
            src_orr: src_orr.to_string(),
            dst_orr: dst_orr.to_string(),
        });
        let resp: GetOrrPathResponse = tokio::time::timeout(self.rpc_timeout, c.get_orr_path(req))
            .await
            .map_err(|_| OrrError::Relay("sdn get_orr_path timeout".into()))?
            .map_err(|s| OrrError::Relay(format!("sdn get_orr_path: {s}")))?
            .into_inner();
        Ok(resp.orrs)
    }
}
