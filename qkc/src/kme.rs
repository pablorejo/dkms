//! Cliente ETSI GS QKD 014 contra el **KME** de un enlace `qkd`: el
//! simulador `quditto` en pruebas, el KME del hardware QKD en producción.
//! Define también [`KeySource`], la abstracción que el `KeyStore` consume,
//! de la que la fuente PQC (`pqc_source`) es la otra implementación.
//!
//! Cada `KmeClient` representa la vista local de un enlace concreto.
//! Los dos QKCs del enlace hablan con el mismo par de KMEs (un KME por
//! extremo en hardware real, un solo proceso en quditto):
//!
//! * `enc_keys` saca N claves frescas (con sus `key_ID`s).
//! * `dec_keys` (POST con batch de `key_ID`s) recupera las claves que
//!   ya entregó el otro lado.
//! * `status` (vía [`KeySource::stock`]) da el nivel del almacén, que
//!   alimenta al estimador de tasa.
//!
//! Negocia wire **binario** (`Accept: application/octet-stream`) con
//! quditto — ahorra base64+JSON — y cae al JSON del estándar con cualquier
//! KME ortodoxo. Con `[tls]` o una credencial por KME (`kme_cert/key/ca`)
//! el canal va en mTLS contra la PKI del KME.
//!
//! Este módulo ya NO hace coalescing. El coalescing antiguo era un
//! parche para ocultar la latencia HTTP en el hot path. Con el
//! `KeyStore` (ver `keystore.rs`), las peticiones HTTP a quditto van
//! en **background** y nunca en el hot path → el batch grande lo
//! decide directamente el `KeyStore`, no este cliente.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

use etsi::{
    v014::{Etsi014Key, Etsi014KeyContainer, Etsi014KeyID, Etsi014Status},
    Base64Bytes,
};
use reqwest::{
    header::{ACCEPT, CONTENT_TYPE},
    Client, StatusCode,
};
use tracing::warn;
use uuid::Uuid;

use crate::error::{QkcError, Result};

/// Una clave OTP completa (id + N bytes según `key_size_bits`).
///
/// `material` es el pad de un solo uso: va en `Zeroizing` para que TODA copia
/// (la cola ENC, el DashMap DEC, los batches en vuelo) se borre de memoria al
/// morir — el resto del árbol de material (épocas PQC, buffers del DKMS,
/// secretos del ORR) ya lo hacía y este era el hueco.
#[derive(Debug, Clone)]
pub struct OtpKey {
    pub key_id: Uuid,
    pub material: zeroize::Zeroizing<Vec<u8>>,
}

impl From<Etsi014Key> for OtpKey {
    fn from(k: Etsi014Key) -> Self {
        Self {
            key_id: k.key_id,
            material: zeroize::Zeroizing::new(k.key.into_inner()),
        }
    }
}

struct KmeInner {
    http: Client,
    base: String,
    sae_id: String,
    /// Tamaño de clave en bits. Arranca con el valor de config y lo **corrige**
    /// [`KmeClient::probe`] con el `key_size` que anuncia el KME: quien manda es
    /// el equipo, no el fichero. Sólo se escribe en el sondeo, antes de que
    /// salga la primera clave, porque el troceador OTP asume un tamaño estable.
    key_size_bits: AtomicU32,
    /// `max_key_per_request` del `/status`. `0` = todavía sin sondear, en cuyo
    /// caso se pide el lote entero de una vez (comportamiento con quditto).
    max_per_request: AtomicU32,
    /// El sondeo ya trajo capacidades buenas; no repetirlo en cada relleno.
    probed: AtomicBool,
    /// El KME acepta varios `key_ID` en un mismo POST de `dec_keys`. ETSI-014
    /// **no** lo anuncia en ningún campo, así que la única forma de saberlo es
    /// intentarlo: al primer rechazo se baja a una petición por clave.
    dec_batch: AtomicBool,
}

#[derive(Clone)]
pub struct KmeClient {
    inner: Arc<KmeInner>,
}

/// Nivel del almacén de claves del KME en un instante.
#[derive(Debug, Clone, Copy)]
pub struct KmeStock {
    /// `stored_key_count` del `/status` ETSI-014.
    pub stored: u64,
    /// `max_key_count` — el techo a partir del cual el KME descarta o pausa.
    pub max: u64,
}

/// Lo que el KME dice de sí mismo en `/status` y que condiciona cómo hay que
/// hablarle. Se descubre solo: al operador le basta con declarar la URL, las
/// credenciales y el `sae_id`.
#[derive(Debug, Clone)]
pub struct KmeCaps {
    /// Tamaño de clave que entrega, en bits.
    pub key_size_bits: u32,
    /// Claves por petición que admite (`1` en el hardware IDQ del laboratorio,
    /// `128` en quditto).
    pub max_key_per_request: u32,
    /// SAE maestro del par, tal y como lo tiene aprovisionado el KME.
    pub master_sae_id: String,
    /// SAE esclavo del par. Conocido uno de los dos, el `/status` da el otro.
    pub slave_sae_id: String,
}

/// Resultado crudo de un POST de `dec_keys`: hace falta distinguir un rechazo
/// (que puede significar «no acepto lotes») de un fallo de transporte.
enum DecOutcome {
    Keys(Vec<OtpKey>),
    Rejected(StatusCode, String),
}

/// Fuente de claves OTP de un enlace QKC↔QKC.
///
/// Abstrae de dónde salen las claves para que el [`KeyStore`] sea
/// agnóstico al tipo de canal:
///
/// * [`KmeClient`] — canal **QKD**: pide al quditto compartido por ETSI 014.
/// * [`crate::pqc_source::PqcKeySource`] — canal **PQC**: deriva un flujo
///   determinista de un secreto ML-KEM compartido (sin red).
///
/// Las dos únicas operaciones del hot path de relleno son `enc_keys`
/// (claves frescas para cifrar) y `dec_keys` (recuperar las que el peer
/// anunció vía `FRAME_KEY_IDS_NOTIFY`). Ambos extremos del enlace
/// obtienen material idéntico para un mismo `key_id`.
///
/// [`KeyStore`]: crate::keystore::KeyStore
#[async_trait::async_trait]
pub trait KeySource: Send + Sync {
    /// Devuelve `number` claves frescas (con sus `key_id`) para cifrar.
    async fn enc_keys(&self, number: u32) -> Result<Vec<OtpKey>>;
    /// Recupera el material de los `key_id` que anunció el peer.
    async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>>;
    /// Nivel del almacén del KME (`/status` ETSI-014), para el estimador de
    /// tasa. `Ok(None)` = la fuente no tiene almacén consultable (PQC deriva
    /// bajo demanda) y el estimador ni arranca; `Err` = KME inalcanzable.
    async fn stock(&self) -> Result<Option<KmeStock>> {
        Ok(None)
    }
}

impl KmeClient {
    pub fn new(
        base: String,
        sae_id: String,
        key_size_bits: u32,
        tls: Option<common::http::ClientTls<'_>>,
    ) -> Result<Self> {
        // HTTP/1.1 con pool grande (compat con simple_quditto Python que
        // no soporta h2c). Para loopback con quditto Rust se podría
        // re-habilitar `http2_prior_knowledge()` cuando el quditto Rust
        // multi-link esté disponible.
        // mTLS hacia el KME cuando la URL es https.
        //
        // ETSI GS QKD 014 exige TLS mutuo entre el consumidor y el KME, y aquí
        // importa más que en ningún otro sitio: TODA la seguridad del modo QKD
        // descansa en que las claves vengan del dispositivo QKD de verdad. Si
        // este canal va en claro, quien lo controle puede suplantar al KME y
        // servir claves propias — y entonces el OTP del enlace cifra
        // perfectamente con una clave que el atacante conoce. Se dejó en HTTP
        // por ser intra-institución (docs/SECURITY.md §1.2), pero es una
        // decisión de despliegue, no una propiedad del sistema.
        let http = match tls {
            Some(t) => common::http::announcer_client(&base, Some(t), Duration::from_secs(30))
                .map_err(|e| QkcError::BadRequest(format!("cliente KME TLS: {e}")))?,
            None => Client::builder()
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(256)
                .timeout(Duration::from_secs(30))
                .tcp_nodelay(true)
                .build()?,
        };
        Ok(Self {
            inner: Arc::new(KmeInner {
                http,
                base: base.trim_end_matches('/').to_string(),
                sae_id,
                key_size_bits: AtomicU32::new(key_size_bits),
                max_per_request: AtomicU32::new(0),
                probed: AtomicBool::new(false),
                dec_batch: AtomicBool::new(true),
            }),
        })
    }

    pub fn key_size_bits(&self) -> u32 {
        self.inner.key_size_bits.load(Ordering::Relaxed)
    }

    /// `GET /status` en crudo. Lo comparten el sondeo de capacidades y el
    /// estimador de tasa, que piden lo mismo con distinto interés.
    async fn fetch_status(&self) -> Result<Etsi014Status> {
        let inner = &self.inner;
        let url = format!("{}/api/v1/keys/{}/status", inner.base, inner.sae_id);
        let resp = inner
            .http
            .get(&url)
            .header(ACCEPT, "application/json")
            .timeout(Duration::from_secs(3))
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(QkcError::Quditto(format!(
                "status {url} -> {status}: {text}"
            )));
        }
        resp.json()
            .await
            .map_err(|e| QkcError::Quditto(format!("status json decode: {e}")))
    }

    /// Descubre cómo hay que hablarle a este KME y se ajusta.
    ///
    /// ETSI-014 obliga a publicar en `/status` el tamaño de clave y el máximo
    /// de claves por petición, así que no hay razón para que el operador los
    /// repita en la config — ni para que el QKC los dé por supuestos. Antes
    /// pedía lotes de 128 y `size` de fichero contra cualquier equipo: con un
    /// KME real (IDQ del laboratorio: `max_key_per_request = 1`, claves de 256
    /// bits) eso son 400 en cada relleno.
    ///
    /// El `key_size` del KME **gana** al de la config: el equipo entrega lo que
    /// entrega, y seguir pidiendo otra cosa sólo produce rechazos. Se avisa,
    /// porque los dos extremos del enlace deben coincidir en el tamaño.
    pub async fn probe(&self) -> Result<KmeCaps> {
        let st = self.fetch_status().await?;
        let inner = &self.inner;

        let configured = inner.key_size_bits.load(Ordering::Relaxed);
        if st.key_size != 0 && st.key_size != configured {
            warn!(
                kme = %inner.base,
                sae_id = %inner.sae_id,
                configured,
                announced = st.key_size,
                "kme.probe: el KME entrega otro tamaño de clave que el configurado; adopto el suyo"
            );
            inner.key_size_bits.store(st.key_size, Ordering::Relaxed);
        }
        if st.max_key_per_request > 0 {
            inner
                .max_per_request
                .store(st.max_key_per_request, Ordering::Relaxed);
        }
        inner.probed.store(true, Ordering::Relaxed);

        Ok(KmeCaps {
            key_size_bits: inner.key_size_bits.load(Ordering::Relaxed),
            max_key_per_request: st.max_key_per_request,
            master_sae_id: st.master_sae_id,
            slave_sae_id: st.slave_sae_id,
        })
    }

    /// Claves por petición admitidas, o `None` si aún no se ha sondeado.
    pub fn max_keys_per_request(&self) -> Option<u32> {
        match self.inner.max_per_request.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Una sola llamada a `enc_keys` pidiendo `number` claves.
    async fn enc_keys_once(&self, number: u32) -> Result<Vec<OtpKey>> {
        let inner = &self.inner;
        let url = format!(
            "{}/api/v1/keys/{}/enc_keys?number={}&size={}",
            inner.base,
            inner.sae_id,
            number,
            inner.key_size_bits.load(Ordering::Relaxed),
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
        Ok(container
            .keys
            .into_iter()
            .map(|k| OtpKey {
                key_id: k.key_id,
                material: zeroize::Zeroizing::new(k.key.into_inner()),
            })
            .collect())
    }

    /// Un solo POST de `dec_keys` con los `key_ID` dados.
    async fn dec_keys_once(&self, ids: &[Uuid]) -> Result<DecOutcome> {
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
            return Ok(DecOutcome::Rejected(status, text));
        }
        let container: Etsi014KeyContainer = resp
            .json()
            .await
            .map_err(|e| QkcError::Quditto(format!("dec_keys json decode: {e}")))?;
        Ok(DecOutcome::Keys(
            container
                .keys
                .into_iter()
                .map(|k| OtpKey {
                    key_id: k.key_id,
                    material: zeroize::Zeroizing::new(k.key.into_inner()),
                })
                .collect(),
        ))
    }
}

#[async_trait::async_trait]
impl KeySource for KmeClient {
    /// Claves frescas para cifrar, respetando lo que el KME admite por
    /// petición.
    ///
    /// El `KeyStore` pide lotes grandes (`REFILL_BATCH`) porque con quditto
    /// salen en una sola llamada. Contra un equipo que sólo sirve `n` claves
    /// por petición, el troceado se hace **aquí**: el almacén sigue pidiendo lo
    /// que necesita y este cliente lo reparte en las llamadas que haga falta.
    /// Se devuelve lo que se haya conseguido —el `KeyStore` acepta lotes
    /// cortos— y sólo se propaga el error si no se logró ni una clave.
    async fn enc_keys(&self, number: u32) -> Result<Vec<OtpKey>> {
        if number == 0 {
            return Ok(vec![]);
        }
        // Primer relleno: averigua con quién se habla antes de pedir nada, para
        // no quemar una tanda de rechazos con el tamaño y el lote equivocados.
        if !self.inner.probed.load(Ordering::Relaxed) {
            let _ = self.probe().await;
        }

        let per_request = match self.inner.max_per_request.load(Ordering::Relaxed) {
            0 => number,
            n => n.min(number).max(1),
        };
        if per_request >= number {
            let keys = self.enc_keys_once(number).await?;
            // Aceptamos batches parciales (n < number): el KME puede estar
            // generando despacio y devolvernos lo que tenga. Rechazar y
            // reintentar deja claves zombi en su buffer `delivered` —
            // preferimos llevarnos lo que sí entregó. Solo 0 sigue siendo
            // error (el caller dormirá 100 ms antes del retry).
            if keys.is_empty() {
                return Err(QkcError::NotEnoughKeys {
                    requested: number,
                    received: 0,
                });
            }
            return Ok(keys);
        }

        let mut out: Vec<OtpKey> = Vec::with_capacity(number as usize);
        while (out.len() as u32) < number {
            let take = per_request.min(number - out.len() as u32);
            match self.enc_keys_once(take).await {
                Ok(keys) if keys.is_empty() => break,
                Ok(mut keys) => out.append(&mut keys),
                // Un fallo a mitad suele ser el KME seco (503). Con material ya
                // recogido, devolverlo es mejor que perderlo.
                Err(e) => {
                    if out.is_empty() {
                        return Err(e);
                    }
                    break;
                }
            }
        }
        if out.is_empty() {
            return Err(QkcError::NotEnoughKeys {
                requested: number,
                received: 0,
            });
        }
        Ok(out)
    }

    /// `GET /api/v1/keys/{sae_id}/status` — para el estimador de tasa sólo
    /// cuentan `stored_key_count` y `max_key_count`, pero de paso se refrescan
    /// las capacidades: el sondeo se pide ~1/s, así que un KME reconfigurado en
    /// caliente se detecta sin tráfico extra.
    async fn stock(&self) -> Result<Option<KmeStock>> {
        let st = self.fetch_status().await?;
        if st.max_key_per_request > 0 {
            self.inner
                .max_per_request
                .store(st.max_key_per_request, Ordering::Relaxed);
        }
        Ok(Some(KmeStock {
            stored: st.stored_key_count,
            max: st.max_key_count,
        }))
    }

    /// Recupera el material que anunció el peer.
    ///
    /// ETSI-014 no publica si el KME admite varios `key_ID` en un POST, así que
    /// se intenta el lote y, si lo rechaza, se baja a una petición por clave y
    /// se recuerda para las siguientes.
    async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        if ids.len() > 1 && self.inner.dec_batch.load(Ordering::Relaxed) {
            match self.dec_keys_once(ids).await? {
                DecOutcome::Keys(keys) => return Ok(keys),
                DecOutcome::Rejected(status, text) => {
                    if status == StatusCode::BAD_REQUEST {
                        warn!(
                            kme = %self.inner.base,
                            ids = ids.len(),
                            "kme.dec_keys: el KME rechaza el lote; paso a una petición por clave"
                        );
                        self.inner.dec_batch.store(false, Ordering::Relaxed);
                    } else {
                        return Err(QkcError::Quditto(format!("dec_keys -> {status}: {text}")));
                    }
                }
            }
        }

        let mut out: Vec<OtpKey> = Vec::with_capacity(ids.len());
        for id in ids {
            match self.dec_keys_once(std::slice::from_ref(id)).await? {
                DecOutcome::Keys(mut keys) => out.append(&mut keys),
                DecOutcome::Rejected(status, text) => {
                    if out.is_empty() {
                        return Err(QkcError::Quditto(format!("dec_keys -> {status}: {text}")));
                    }
                    break;
                }
            }
        }
        Ok(out)
    }
}

/// Helper para tests: envuelve un `Vec<u8>` en `Base64Bytes`.
pub fn b64_bytes(v: Vec<u8>) -> Base64Bytes {
    Base64Bytes::new(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// KME de mentira con los límites del hardware real: una clave por
    /// petición y `dec_keys` que rechaza cualquier lote. Cuenta las llamadas
    /// para poder afirmar que el troceado ocurre.
    struct FakeKme {
        enc_calls: Arc<AtomicUsize>,
        dec_calls: Arc<AtomicUsize>,
        max_per_request: u32,
        key_size: u32,
        dec_batch_ok: bool,
    }

    async fn spawn_kme(k: FakeKme) -> String {
        use axum::{
            extract::{Path, Query, State},
            routing::get,
            Json, Router,
        };
        use std::collections::HashMap;

        let k = Arc::new(k);
        let status_body = {
            let k = k.clone();
            move || {
                serde_json::json!({
                    "source_KME_ID": "fake", "target_KME_ID": "fake",
                    "master_SAE_ID": "ETSIA", "slave_SAE_ID": "ETSIB",
                    "key_size": k.key_size,
                    "stored_key_count": 100u64, "max_key_count": 100u64,
                    "max_key_per_request": k.max_per_request,
                    "max_key_size": k.key_size, "min_key_size": k.key_size,
                    "max_SAE_ID_count": 0,
                })
            }
        };

        let enc_state = k.clone();
        let dec_state = k.clone();
        let app = Router::new()
            .route(
                "/api/v1/keys/:sae/status",
                get(move |Path(_s): Path<String>| {
                    let b = status_body();
                    async move { Json(b) }
                }),
            )
            .route(
                "/api/v1/keys/:sae/enc_keys",
                get(
                    move |Path(_s): Path<String>,
                          Query(q): Query<HashMap<String, String>>,
                          State(k): State<Arc<FakeKme>>| async move {
                        k.enc_calls.fetch_add(1, Ordering::Relaxed);
                        let n: u32 = q.get("number").and_then(|v| v.parse().ok()).unwrap_or(1);
                        // Igual que el equipo real: más de lo permitido es 400.
                        if n > k.max_per_request {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"message": "Bad request format."})),
                            );
                        }
                        let keys: Vec<_> = (0..n)
                            .map(|_| {
                                serde_json::json!({
                                    "key_ID": Uuid::new_v4(),
                                    "key": base64_of(vec![7u8; (k.key_size / 8) as usize]),
                                })
                            })
                            .collect();
                        (
                            axum::http::StatusCode::OK,
                            Json(serde_json::json!({ "keys": keys })),
                        )
                    },
                ),
            )
            .route(
                "/api/v1/keys/:sae/dec_keys",
                axum::routing::post(
                    move |Path(_s): Path<String>,
                          State(k): State<Arc<FakeKme>>,
                          Json(body): Json<serde_json::Value>| async move {
                        k.dec_calls.fetch_add(1, Ordering::Relaxed);
                        let ids = body["key_IDs"].as_array().cloned().unwrap_or_default();
                        if ids.len() > 1 && !k.dec_batch_ok {
                            return (
                                axum::http::StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({"message": "Bad request format."})),
                            );
                        }
                        let keys: Vec<_> = ids
                            .iter()
                            .map(|e| {
                                serde_json::json!({
                                    "key_ID": e["key_ID"],
                                    "key": base64_of(vec![9u8; (k.key_size / 8) as usize]),
                                })
                            })
                            .collect();
                        (
                            axum::http::StatusCode::OK,
                            Json(serde_json::json!({ "keys": keys })),
                        )
                    },
                ),
            )
            .with_state(k.clone())
            .with_state(enc_state)
            .with_state(dec_state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    fn base64_of(v: Vec<u8>) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(v)
    }

    /// Un KME que sólo sirve una clave por petición debe rellenarse igual: el
    /// cliente trocea el lote que le pide el `KeyStore`. Antes se mandaba
    /// `number=128` de una vez y el equipo real contestaba 400 en cada relleno.
    #[tokio::test]
    async fn a_kme_that_serves_one_key_per_request_is_still_filled_in_batches() {
        let enc_calls = Arc::new(AtomicUsize::new(0));
        let base = spawn_kme(FakeKme {
            enc_calls: enc_calls.clone(),
            dec_calls: Arc::new(AtomicUsize::new(0)),
            max_per_request: 1,
            key_size: 256,
            dec_batch_ok: true,
        })
        .await;

        let c = KmeClient::new(base, "ETSIB".into(), 1024, None).unwrap();
        let keys = c.enc_keys(16).await.unwrap();

        assert_eq!(keys.len(), 16, "se piden 16 aunque vayan de una en una");
        // 16 de enc + el sondeo inicial no cuenta aquí (va a /status).
        assert_eq!(enc_calls.load(Ordering::Relaxed), 16);
        // El tamaño del fichero (1024) cede ante lo que anuncia el equipo.
        assert_eq!(c.key_size_bits(), 256);
        assert_eq!(c.max_keys_per_request(), Some(1));
    }

    /// Con un KME que sí admite lotes se hace UNA sola llamada: no se paga el
    /// troceado cuando no hace falta (quditto anuncia 128).
    #[tokio::test]
    async fn a_kme_that_accepts_batches_is_asked_only_once() {
        let enc_calls = Arc::new(AtomicUsize::new(0));
        let base = spawn_kme(FakeKme {
            enc_calls: enc_calls.clone(),
            dec_calls: Arc::new(AtomicUsize::new(0)),
            max_per_request: 128,
            key_size: 256,
            dec_batch_ok: true,
        })
        .await;

        let c = KmeClient::new(base, "ETSIB".into(), 256, None).unwrap();
        let keys = c.enc_keys(16).await.unwrap();

        assert_eq!(keys.len(), 16);
        assert_eq!(enc_calls.load(Ordering::Relaxed), 1);
    }

    /// ETSI-014 no dice si `dec_keys` acepta varios ids, así que se prueba y,
    /// ante el rechazo, se baja a una petición por clave — y se recuerda, para
    /// no volver a comerse un 400 en cada mensaje.
    #[tokio::test]
    async fn a_kme_that_rejects_dec_batches_falls_back_to_one_request_per_key() {
        let dec_calls = Arc::new(AtomicUsize::new(0));
        let base = spawn_kme(FakeKme {
            enc_calls: Arc::new(AtomicUsize::new(0)),
            dec_calls: dec_calls.clone(),
            max_per_request: 1,
            key_size: 256,
            dec_batch_ok: false,
        })
        .await;

        let c = KmeClient::new(base, "ETSIB".into(), 256, None).unwrap();
        let ids: Vec<Uuid> = (0..4).map(|_| Uuid::new_v4()).collect();

        let keys = c.dec_keys(&ids).await.unwrap();
        assert_eq!(keys.len(), 4, "las recupera todas pese al rechazo del lote");
        // 1 intento de lote (rechazado) + 4 individuales.
        assert_eq!(dec_calls.load(Ordering::Relaxed), 5);

        // La segunda vez ya no reintenta el lote: 4 llamadas, no 5.
        dec_calls.store(0, Ordering::Relaxed);
        let keys = c.dec_keys(&ids).await.unwrap();
        assert_eq!(keys.len(), 4);
        assert_eq!(dec_calls.load(Ordering::Relaxed), 4);
    }
}
