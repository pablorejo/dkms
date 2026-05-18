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
    GetPathsWithRatiosRequest, GetPathsWithRatiosResponse, StreamTopologyRequest,
    TopologyEvent,
};
use tonic::Streaming;

use crate::error::{OrrError, Result};

/// Path con su ratio y rate absoluto, devuelto por el solver
/// K-Splittable MCF del SDN. El campo `qkc_hops` es la lista
/// `src_qkc → … → dst_qkc` (ambos extremos incluidos).
#[derive(Debug, Clone)]
pub struct PathWithRatio {
    pub qkc_hops: Vec<String>,
    pub omega: f64,
    pub keys_per_second: f64,
}

/// Respuesta completa del RPC `GetPathsWithRatios`. Si `paths` está
/// vacío y `total_keys_per_second == 0`, el flow está en Saturated o
/// el commodity no existe — el caller debe hacer fallback a
/// single-path (`get_orr_path`) o no enviar.
#[derive(Debug, Clone, Default)]
pub struct PathsWithRatios {
    pub paths: Vec<PathWithRatio>,
    pub total_keys_per_second: f64,
}

#[derive(Clone)]
pub struct SdnClient {
    channel: Channel,
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

    /// K-Splittable MCF: pide al SDN los K paths (con ratios) para el
    /// commodity `(src_dkms → dst_dkms)`. El caller los cachea y
    /// muestrea por alias method en cada envío.
    ///
    /// La respuesta es vacía si:
    /// - El flow está Saturated (rate=0 en el snapshot).
    /// - `src_dkms == dst_dkms`.
    /// - Ambos DKMS están anclados al mismo QKC físico.
    ///
    /// En esos casos el caller debe fallback a `get_orr_path` o no
    /// enviar.
    pub async fn get_paths_with_ratios(
        &self,
        src_dkms: &str,
        dst_dkms: &str,
    ) -> Result<PathsWithRatios> {
        let mut c = SdnControlClient::new(self.channel.clone());
        let req = tonic::Request::new(GetPathsWithRatiosRequest {
            src_dkms: src_dkms.to_string(),
            dst_dkms: dst_dkms.to_string(),
        });
        let resp: GetPathsWithRatiosResponse = tokio::time::timeout(
            self.rpc_timeout,
            c.get_paths_with_ratios(req),
        )
        .await
        .map_err(|_| OrrError::Relay("sdn get_paths_with_ratios timeout".into()))?
        .map_err(|s| OrrError::Relay(format!("sdn get_paths_with_ratios: {s}")))?
        .into_inner();
        Ok(PathsWithRatios {
            paths: resp
                .paths
                .into_iter()
                .map(|p| PathWithRatio {
                    qkc_hops: p.qkc_hops,
                    omega: p.omega,
                    keys_per_second: p.keys_per_second,
                })
                .collect(),
            total_keys_per_second: resp.total_keys_per_second,
        })
    }
}
