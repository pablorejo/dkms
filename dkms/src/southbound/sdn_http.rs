//! Cliente HTTP del DKMS hacia el SDN para endpoints que no están en
//! gRPC (de momento solo `GET /rate/{dkms_id}`).
//!
//! El SDN expone un endpoint REST con las rates per-peer y per-role
//! computadas por el solver MCF. El DKMS hace polling cada N segundos y
//! cachea las rates en su `Generator` para alimentar los token buckets
//! per-peer.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct SdnHttpClient {
    base_url: String,
    http: reqwest::Client,
    rpc_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PeerRate {
    pub enc: f64,
    pub dec: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DkmsRatesResponse {
    pub dkms_id: String,
    pub topology_version: i64,
    pub peers: HashMap<String, PeerRate>,
}

impl SdnHttpClient {
    /// Construye el cliente. `base_url` debe ser un URL absoluto sin la
    /// barra final, p.ej. `http://127.0.0.1:50055`.
    pub fn new(base_url: impl Into<String>, rpc_timeout: Duration) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(rpc_timeout)
            .build()
            .map_err(|e| anyhow!("reqwest builder: {e}"))?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            http,
            rpc_timeout,
        })
    }

    /// `GET /rate/{dkms_id}`. Devuelve las rates ENC/DEC per peer.
    /// Si el DKMS no es conocido por el SDN devuelve un mapa vacío.
    pub async fn get_rates(&self, dkms_id: &str) -> Result<DkmsRatesResponse> {
        let url = format!("{}/rate/{}", self.base_url, dkms_id);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|e| anyhow!("GET {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("GET {url} returned {status}: {body}"));
        }
        let parsed: DkmsRatesResponse = resp
            .json()
            .await
            .map_err(|e| anyhow!("decode JSON from {url}: {e}"))?;
        Ok(parsed)
    }

    pub fn rpc_timeout(&self) -> Duration {
        self.rpc_timeout
    }

    /// `POST /priority`. Manda en batch los cambios de clase QoS de los
    /// buffers de este DKMS. Cada update lleva (dkms_id, peer, role,
    /// class). El SDN actualiza su `BufferPriorityRegistry` y recomputa
    /// MCF inmediatamente — el siguiente `get_rates` ya verá los nuevos
    /// rates.
    pub async fn post_priority(&self, updates: &[PriorityUpdate]) -> Result<PriorityApplied> {
        if updates.is_empty() {
            return Ok(PriorityApplied {
                applied: 0,
                errors: vec![],
            });
        }
        let url = format!("{}/priority", self.base_url);
        let body = serde_json::json!({"updates": updates});
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow!("POST {url}: {e}"))?;
        let status = resp.status();
        if !status.is_success() && status.as_u16() != 206 {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("POST {url} returned {status}: {body}"));
        }
        let parsed: PriorityApplied = resp
            .json()
            .await
            .map_err(|e| anyhow!("decode JSON from {url}: {e}"))?;
        Ok(parsed)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PriorityUpdate {
    pub dkms_id: String,
    pub peer: String,
    pub role: String,  // "enc_keys" | "dec_keys"
    pub class: String, // "priority" | "important" | "quickly" | "relax" | "best_effort" | "saturated"
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriorityApplied {
    pub applied: u32,
    #[serde(default)]
    pub errors: Vec<String>,
}
