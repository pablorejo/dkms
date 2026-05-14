//! Cliente HTTP contra el **quditto compartido** de un enlace.
//!
//! Cada `KmeClient` representa la vista local de un enlace concreto.
//! Como los dos QKCs del enlace apuntan al mismo proceso quditto:
//!
//! * `enc_keys` saca N claves frescas (con sus `key_ID`s).
//! * `dec_keys` (POST con batch de `key_ID`s) recupera las claves que
//!   ya entregó el otro lado.
//!
//! Negocia wire **binario** (`Accept: application/octet-stream`) por
//! defecto — ahorra base64+JSON, compatible con clientes ETSI 014
//! ortodoxos en el quditto (que cae a JSON si no ven Accept binario).
//!
//! Este módulo ya NO hace coalescing. El coalescing antiguo era un
//! parche para ocultar la latencia HTTP en el hot path. Con el
//! `KeyStore` (ver `keystore.rs`), las peticiones HTTP a quditto van
//! en **background** y nunca en el hot path → el batch grande lo
//! decide directamente el `KeyStore`, no este cliente.

use std::{sync::Arc, time::Duration};

use etsi::{binary, v014::Etsi014Key, Base64Bytes};
use reqwest::{
    header::{ACCEPT, CONTENT_TYPE},
    Client,
};
use uuid::Uuid;

use crate::error::{QkcError, Result};

/// Una clave OTP completa (id + N bytes según `key_size_bits`).
#[derive(Debug, Clone)]
pub struct OtpKey {
    pub key_id: Uuid,
    pub material: Vec<u8>,
}

impl From<Etsi014Key> for OtpKey {
    fn from(k: Etsi014Key) -> Self {
        Self {
            key_id: k.key_id,
            material: k.key.into_inner(),
        }
    }
}

struct KmeInner {
    http: Client,
    base: String,
    sae_id: String,
    key_size_bits: u32,
}

#[derive(Clone)]
pub struct KmeClient {
    inner: Arc<KmeInner>,
}

impl KmeClient {
    pub fn new(base: String, sae_id: String, key_size_bits: u32) -> Result<Self> {
        // HTTP/2 prior knowledge (h2c) + connection pool grande, sin
        // TLS para loopback. Para producción cambiar a HTTPS+ALPN.
        let http = Client::builder()
            .http2_prior_knowledge()
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_adaptive_window(true)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(256)
            .timeout(Duration::from_secs(30))
            .tcp_nodelay(true)
            .build()?;
        Ok(Self {
            inner: Arc::new(KmeInner {
                http,
                base: base.trim_end_matches('/').to_string(),
                sae_id,
                key_size_bits,
            }),
        })
    }

    pub fn key_size_bits(&self) -> u32 {
        self.inner.key_size_bits
    }

    /// `GET /api/v1/keys/{sae_id}/enc_keys?number=N&size=B` con
    /// `Accept: application/octet-stream`.
    pub async fn enc_keys(&self, number: u32) -> Result<Vec<OtpKey>> {
        if number == 0 {
            return Ok(vec![]);
        }
        let inner = &self.inner;
        let url = format!(
            "{}/api/v1/keys/{}/enc_keys?number={}&size={}",
            inner.base, inner.sae_id, number, inner.key_size_bits,
        );
        let resp = inner
            .http
            .get(&url)
            .header(ACCEPT, binary::CONTENT_TYPE)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(QkcError::Quditto(format!(
                "enc_keys {url} -> {status}: {text}"
            )));
        }
        let bytes = resp.bytes().await?;
        let pairs = binary::unpack_keys(&bytes)
            .map_err(|e| QkcError::Quditto(format!("enc_keys binary decode: {e}")))?;
        let n = pairs.len() as u32;
        if n < number {
            return Err(QkcError::NotEnoughKeys {
                requested: number,
                received: n,
            });
        }
        Ok(pairs
            .into_iter()
            .map(|(id, mat)| OtpKey { key_id: id, material: mat })
            .collect())
    }

    /// `POST /api/v1/keys/{sae_id}/dec_keys` con body+accept binarios.
    /// Recupera las claves indicadas — todas en una sola request.
    pub async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let inner = &self.inner;
        let url = format!("{}/api/v1/keys/{}/dec_keys", inner.base, inner.sae_id);
        let body = binary::pack_key_ids(ids);
        let resp = inner
            .http
            .post(&url)
            .header(CONTENT_TYPE, binary::CONTENT_TYPE)
            .header(ACCEPT, binary::CONTENT_TYPE)
            .body(body)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(QkcError::Quditto(format!(
                "dec_keys {url} -> {status}: {text}"
            )));
        }
        let bytes = resp.bytes().await?;
        let pairs = binary::unpack_keys(&bytes)
            .map_err(|e| QkcError::Quditto(format!("dec_keys binary decode: {e}")))?;
        Ok(pairs
            .into_iter()
            .map(|(id, mat)| OtpKey { key_id: id, material: mat })
            .collect())
    }
}

/// Helper para tests: envuelve un `Vec<u8>` en `Base64Bytes`.
pub fn b64_bytes(v: Vec<u8>) -> Base64Bytes {
    Base64Bytes::new(v)
}
