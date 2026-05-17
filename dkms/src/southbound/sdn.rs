//! Cliente gRPC del DKMS contra la SDN.
//!
//! Wraps thin de [`tonic`]; lo único interesante aquí es:
//!
//! * Construye una vez el `Channel` (multiplex HTTP/2 reutilizado) y clona
//!   el `SdnControlClient` por llamada — la clonación de tonic es barata.
//! * Aplica `connect_timeout` y `timeout` por RPC desde [`crate::config`].
//! * Convierte `tonic::Status` en `DkmsError::SdnUnreachable` para no
//!   contaminar el código de servicio con detalles del transporte.
//!
//! NOTA: la SDN aún no expone un RPC de *SAE binding* en el proto. Cuando
//! lo añada, el método `resolve_sae` aquí pasará a llamarlo en vez de
//! devolver `SaeBindingLookupFailed`. Por ahora un [`crate::sae_binding::SaeResolver`]
//! estático cubre la fase de desarrollo.

use std::time::Duration;

use anyhow::anyhow;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::debug;

use common::ids::NodeId;
use common::proto::{
    common::v1::NodeId as ProtoNodeId,
    sdn::v1::{
        sdn_control_client::SdnControlClient, ComputePathRequest, ComputePathResponse, DkmsMetric,
        GetSaeBindingRequest, GetSaeBindingResponse, PathPolicy, StreamTopologyRequest,
        TopologyEvent,
    },
};
use tonic::Streaming;

use crate::{
    config::SouthboundCfg,
    error::{DkmsError, Result},
};

#[derive(Clone)]
pub struct SdnClient {
    channel: Channel,
    rpc_timeout: Duration,
}

impl SdnClient {
    pub async fn connect(cfg: &SouthboundCfg, tls: Option<ClientTlsConfig>) -> Result<Self> {
        let mut endpoint = Endpoint::from_shared(cfg.sdn_endpoint.clone())
            .map_err(|e| DkmsError::SdnUnreachable(anyhow!(e)))?
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            .timeout(Duration::from_millis(cfg.rpc_timeout_ms));

        if let Some(t) = tls {
            endpoint = endpoint
                .tls_config(t)
                .map_err(|e| DkmsError::SdnUnreachable(anyhow!(e)))?;
        }

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| DkmsError::SdnUnreachable(anyhow!(e)))?;

        debug!(endpoint = %cfg.sdn_endpoint, "sdn client connected");
        Ok(Self {
            channel,
            rpc_timeout: Duration::from_millis(cfg.rpc_timeout_ms),
        })
    }

    fn client(&self) -> SdnControlClient<Channel> {
        SdnControlClient::new(self.channel.clone())
    }

    pub async fn compute_path(
        &self,
        src: &NodeId,
        dst: &NodeId,
        required_bps: u64,
    ) -> Result<ComputePathResponse> {
        let mut c = self.client();
        let req = tonic::Request::new(ComputePathRequest {
            src: Some(ProtoNodeId {
                value: src.to_string(),
            }),
            dst: Some(ProtoNodeId {
                value: dst.to_string(),
            }),
            required_bps,
            policy: PathPolicy::MinCostFlow as i32,
        });
        let resp = tokio::time::timeout(self.rpc_timeout, c.compute_path(req))
            .await
            .map_err(|_| DkmsError::SdnUnreachable(anyhow!("compute_path timeout")))?
            .map_err(|s| DkmsError::SdnUnreachable(anyhow!(s)))?;
        Ok(resp.into_inner())
    }

    /// Resuelve `sae_id → dkms_id` consultando la SDN. El caller debe
    /// envolverlo en `SaeBindingCache` para no preguntar en cada
    /// request — la cache TTL absorbe las hits repetidas.
    pub async fn get_sae_binding(&self, sae_id: &str) -> Result<GetSaeBindingResponse> {
        let mut c = self.client();
        let req = tonic::Request::new(GetSaeBindingRequest {
            sae_id: sae_id.to_string(),
        });
        let resp = tokio::time::timeout(self.rpc_timeout, c.get_sae_binding(req))
            .await
            .map_err(|_| DkmsError::SdnUnreachable(anyhow!("get_sae_binding timeout")))?
            .map_err(|s| DkmsError::SdnUnreachable(anyhow!(s)))?;
        Ok(resp.into_inner())
    }

    /// Abre el stream `StreamTopology`. El caller drena los eventos
    /// para invalidar cachés locales (SAE bindings, etc.).
    pub async fn stream_topology(&self) -> Result<Streaming<TopologyEvent>> {
        let mut c = self.client();
        let req = tonic::Request::new(StreamTopologyRequest { since_version: 0 });
        let stream = c
            .stream_topology(req)
            .await
            .map_err(|s| DkmsError::SdnUnreachable(anyhow!(s)))?
            .into_inner();
        Ok(stream)
    }

    /// Push de una métrica de delivery al SDN (informativo, fire-and-forget).
    pub async fn report_metric(&self, metric: DkmsMetric) -> Result<()> {
        let mut c = self.client();
        // ReportDkmsMetrics es un stream cliente; para una métrica suelta
        // abrimos un stream de un solo elemento.
        let stream = tokio_stream::once(metric);
        let req = tonic::Request::new(stream);
        tokio::time::timeout(self.rpc_timeout, c.report_dkms_metrics(req))
            .await
            .map_err(|_| DkmsError::SdnUnreachable(anyhow!("report_metric timeout")))?
            .map_err(|s| DkmsError::SdnUnreachable(anyhow!(s)))?;
        Ok(())
    }
}
