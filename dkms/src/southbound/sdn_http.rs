//! Cliente HTTP del DKMS hacia el admin de la SDN, para lo que no va por
//! gRPC: `GET /rate/{dkms_id}` y `POST /demand`.
//!
//! La SDN publica por REST las tasas por peer y por rol que calcula el
//! asignador de rates; el DKMS las sondea cada N segundos y las cachea en
//! su `Generator` para alimentar los token buckets por peer. En sentido
//! contrario reporta, por lote, la demanda medida de cada par
//! (`demand_tracker`), que es la entrada del asignador.

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
    /// El SDN ve un camino estrictamente-QKD a este peer (mismo componente
    /// del subgrafo QKD). Si no, una clave QKD-grade no es entregable y una
    /// petición `strict_qkd` a este destino debe rechazarse (4xx). Default
    /// `true` (no rechazar) para tolerar un SDN antiguo que no emita el campo.
    #[serde(default = "default_true")]
    pub qkd_available: bool,
    /// Existe algún camino (grafo completo, incl. PQC) a este peer.
    #[serde(default = "default_true")]
    pub reachable: bool,
}

fn default_true() -> bool {
    true
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
        Self::new_with_tls(base_url, rpc_timeout, None)
    }

    /// Igual que [`Self::new`] pero con material mTLS de cliente: se usa si el
    /// `base_url` es `https://` (si no, se ignora y el cliente va en claro).
    pub fn new_with_tls(
        base_url: impl Into<String>,
        rpc_timeout: Duration,
        tls: Option<common::http::ClientTls<'_>>,
    ) -> Result<Self> {
        let base = base_url.into().trim_end_matches('/').to_string();
        let http = common::http::announcer_client(&base, tls, rpc_timeout)
            .map_err(|e| anyhow!("sdn http client: {e}"))?;
        Ok(Self {
            base_url: base,
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
}

// --- demand reporting (MCMCF-λ) ----------------------------------------

/// Mirror of `sdn::demand::CommodityDemand` — kept here so the DKMS
/// crate doesn't have to import the `sdn` crate just for this struct.
/// Fields are the same; mismatches would be a wire-protocol break and
/// would show up immediately in integration tests.
#[derive(Debug, Clone, Serialize)]
pub struct CommodityDemand {
    pub src_dkms: String,
    pub dst_dkms: String,
    pub level: f64,
    pub capacity: f64,
    pub drain_rate: f64,
    pub timestamp_ms: i64,
    /// Security grade of this demand (serialises to "qkd"/"pqc"). The SDN keys
    /// its registry by (src,dst,grade) and builds a per-grade commodity.
    pub grade: common::security::KeyGrade,
}

/// Wire payload for `POST /demand` — one batch per DKMS reporting all
/// of its outgoing commodities.
#[derive(Debug, Clone, Serialize)]
pub struct DemandReport {
    pub dkms_id: String,
    pub entries: Vec<CommodityDemand>,
}

/// Response from `POST /demand`. Accepted = entries persisted in the
/// SDN's `DemandRegistry`. Errors are per-entry reasons (not a global
/// failure) — the SDN returns 206 in that case but we still surface
/// it as `Ok` to the caller; the warn log captures the detail.
#[derive(Debug, Clone, Deserialize)]
pub struct DemandApplied {
    pub accepted: u32,
    #[serde(default)]
    pub errors: Vec<String>,
    #[serde(default)]
    pub registry_len: u32,
}

impl SdnHttpClient {
    /// `POST /demand`. Empty batches are short-circuited (no network
    /// I/O) so the caller can iterate over the known-peer set without
    /// pre-filtering.
    pub async fn post_demand(&self, report: &DemandReport) -> Result<DemandApplied> {
        if report.entries.is_empty() {
            return Ok(DemandApplied {
                accepted: 0,
                errors: vec![],
                registry_len: 0,
            });
        }
        let url = format!("{}/demand", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(report)
            .send()
            .await
            .map_err(|e| anyhow!("POST {url}: {e}"))?;
        let status = resp.status();
        // Accept 200 (all ok) and 206 (partial — some entries rejected).
        if !status.is_success() && status.as_u16() != 206 {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("POST {url} returned {status}: {body}"));
        }
        let parsed: DemandApplied = resp
            .json()
            .await
            .map_err(|e| anyhow!("decode JSON from {url}: {e}"))?;
        Ok(parsed)
    }
}

// ----------------------------------------------------------------- tests
#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use super::*;
    use axum::{
        extract::State, http::StatusCode, response::IntoResponse, routing::post, Json, Router,
    };
    use parking_lot::Mutex;
    use serde_json::Value;
    use tokio::net::TcpListener;

    /// Mock SDN that records every `POST /demand` body it receives.
    #[derive(Clone, Default)]
    struct MockSdn {
        seen: Arc<Mutex<Vec<Value>>>,
        n_calls: Arc<AtomicUsize>,
    }

    async fn handle_demand(State(s): State<MockSdn>, Json(v): Json<Value>) -> impl IntoResponse {
        s.seen.lock().push(v);
        s.n_calls.fetch_add(1, Ordering::Relaxed);
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "accepted": 2u32,
                "errors": Vec::<String>::new(),
                "registry_len": 2u32,
            })),
        )
    }

    /// Bind a tiny mock SDN on 127.0.0.1:0 and return `(client, mock)`.
    async fn spawn_mock_sdn() -> (SdnHttpClient, MockSdn) {
        let mock = MockSdn::default();
        let app = Router::new()
            .route("/demand", post(handle_demand))
            .with_state(mock.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let client =
            SdnHttpClient::new(format!("http://{addr}"), std::time::Duration::from_secs(2))
                .unwrap();
        (client, mock)
    }

    fn demand_entry(src: &str, dst: &str, level: f64, drain: f64) -> CommodityDemand {
        CommodityDemand {
            src_dkms: src.into(),
            dst_dkms: dst.into(),
            level,
            capacity: 4096.0,
            drain_rate: drain,
            timestamp_ms: 1_000,
            grade: common::security::KeyGrade::Qkd,
        }
    }

    #[tokio::test]
    async fn post_demand_serialises_full_body_and_hits_endpoint() {
        let (client, mock) = spawn_mock_sdn().await;
        let report = DemandReport {
            dkms_id: "dA".into(),
            entries: vec![
                demand_entry("dA", "dB", 100.0, 30.0),
                demand_entry("dA", "dC", 200.0, 50.0),
            ],
        };
        let applied = client.post_demand(&report).await.unwrap();
        assert_eq!(applied.accepted, 2);
        assert_eq!(applied.errors.len(), 0);
        assert_eq!(applied.registry_len, 2);
        assert_eq!(mock.n_calls.load(Ordering::Relaxed), 1);
        let seen = mock.seen.lock();
        let body = &seen[0];
        assert_eq!(body["dkms_id"], "dA");
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["src_dkms"], "dA");
        assert_eq!(entries[0]["dst_dkms"], "dB");
        assert_eq!(entries[0]["level"], 100.0);
        assert_eq!(entries[0]["capacity"], 4096.0);
        assert_eq!(entries[0]["drain_rate"], 30.0);
        assert_eq!(entries[0]["timestamp_ms"], 1_000);
    }

    #[tokio::test]
    async fn post_demand_empty_batch_is_no_op_no_network() {
        // No mock — if we hit the network this would error out on
        // connection refused. We rely on the short-circuit inside
        // post_demand to skip the I/O.
        let client =
            SdnHttpClient::new("http://127.0.0.1:1", std::time::Duration::from_secs(1)).unwrap();
        let report = DemandReport {
            dkms_id: "dA".into(),
            entries: vec![],
        };
        let r = client.post_demand(&report).await.unwrap();
        assert_eq!(r.accepted, 0);
        assert_eq!(r.errors.len(), 0);
    }

    #[tokio::test]
    async fn post_demand_accepts_206_partial_response() {
        // Mock returns 206 with errors — should still parse to Ok.
        async fn handle_206(State(_s): State<MockSdn>, Json(_v): Json<Value>) -> impl IntoResponse {
            (
                StatusCode::PARTIAL_CONTENT,
                Json(serde_json::json!({
                    "accepted": 1u32,
                    "errors": ["bad entry"],
                    "registry_len": 5u32,
                })),
            )
        }
        let mock = MockSdn::default();
        let app = Router::new()
            .route("/demand", post(handle_206))
            .with_state(mock);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        let client =
            SdnHttpClient::new(format!("http://{addr}"), std::time::Duration::from_secs(2))
                .unwrap();
        let report = DemandReport {
            dkms_id: "dA".into(),
            entries: vec![demand_entry("dA", "dB", 1.0, 0.0)],
        };
        let r = client.post_demand(&report).await.unwrap();
        assert_eq!(r.accepted, 1);
        assert_eq!(r.errors, vec!["bad entry".to_string()]);
        assert_eq!(r.registry_len, 5);
    }
}
