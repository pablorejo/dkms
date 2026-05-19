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

use etsi::{
    v014::{Etsi014Key, Etsi014KeyContainer, Etsi014KeyID},
    Base64Bytes,
};
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
        // HTTP/1.1 con pool grande (compat con simple_quditto Python que
        // no soporta h2c). Para loopback con quditto Rust se podría
        // re-habilitar `http2_prior_knowledge()` cuando el quditto Rust
        // multi-link esté disponible.
        let http = Client::builder()
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
    /// `Accept: application/json` (compat con simple_quditto Python que solo
    /// sirve ETSI 014 JSON; el path binario `application/octet-stream` se
    /// reactivará cuando el quditto Rust multi-link esté disponible).
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
            .header(ACCEPT, "application/json")
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(QkcError::Quditto(format!(
                "enc_keys {url} -> {status}: {text}"
            )));
        }
        let container: Etsi014KeyContainer = resp
            .json()
            .await
            .map_err(|e| QkcError::Quditto(format!("enc_keys json decode: {e}")))?;
        let pairs: Vec<(Uuid, Vec<u8>)> = container
            .keys
            .into_iter()
            .map(|k| (k.key_id, k.key.into_inner()))
            .collect();
        // Aceptamos batches parciales (n < number): el quditto puede
        // estar generando a R0 lento y devolvernos lo que tenga.
        // Rechazar y reintentar deja claves zombi en su buffer
        // `delivered` — preferimos llevarnos lo que sí entregó.
        // Solo 0 sigue siendo error (significa que el quditto está
        // realmente seco; el caller dormirá 100 ms antes del retry).
        if pairs.is_empty() {
            return Err(QkcError::NotEnoughKeys {
                requested: number,
                received: 0,
            });
        }
        Ok(pairs
            .into_iter()
            .map(|(id, mat)| OtpKey {
                key_id: id,
                material: mat,
            })
            .collect())
    }

    /// `POST /api/v1/keys/{sae_id}/dec_keys` con body JSON ETSI 014.
    pub async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let inner = &self.inner;
        let url = format!("{}/api/v1/keys/{}/dec_keys", inner.base, inner.sae_id);
        let key_ids: Vec<Etsi014KeyID> = ids.iter().map(|u| Etsi014KeyID::new(*u)).collect();
        let resp = inner
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&serde_json::json!({"key_IDs": key_ids}))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(QkcError::Quditto(format!(
                "dec_keys {url} -> {status}: {text}"
            )));
        }
        let container: Etsi014KeyContainer = resp
            .json()
            .await
            .map_err(|e| QkcError::Quditto(format!("dec_keys json decode: {e}")))?;
        let pairs: Vec<(Uuid, Vec<u8>)> = container
            .keys
            .into_iter()
            .map(|k| (k.key_id, k.key.into_inner()))
            .collect();
        Ok(pairs
            .into_iter()
            .map(|(id, mat)| OtpKey {
                key_id: id,
                material: mat,
            })
            .collect())
    }
}

/// Helper para tests: envuelve un `Vec<u8>` en `Base64Bytes`.
pub fn b64_bytes(v: Vec<u8>) -> Base64Bytes {
    Base64Bytes::new(v)
}
