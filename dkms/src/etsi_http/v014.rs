//! Handlers ETSI GS QKD 014 (SAE-facing).
//!
//! Los tres endpoints clásicos, con los dos métodos que permite la spec
//! (V1.1.1, tabla 2: "Get key" y "Get key with key IDs" aceptan POST y,
//! para peticiones simples, también GET — es lo que usa strongSwan):
//!
//! * `GET  /api/v1/keys/{slave_SAE_ID}/status`
//! * `POST /api/v1/keys/{slave_SAE_ID}/enc_keys`   cuerpo JSON completo
//! * `GET  /api/v1/keys/{slave_SAE_ID}/enc_keys`   `?number=N&size=S` (§6.2)
//! * `POST /api/v1/keys/{master_SAE_ID}/dec_keys`  cuerpo JSON completo
//! * `GET  /api/v1/keys/{master_SAE_ID}/dec_keys`  `?key_ID=<uuid>` (§6.4,
//!   solo UNA key y sin extensiones — la propia spec limita el GET a eso)
//!
//! Ambos métodos convergen en el mismo camino de servicio: el GET solo
//! construye el `Etsi014KeyRequest`/`Etsi014KeyIDs` equivalente. Defaults
//! del GET según la spec: `number=1`, `size` = el `key_size` que anuncia
//! Status (256) — idénticos a los defaults serde del cuerpo POST.
//!
//! La autenticación se inyecta vía [`SaePeer`] (extractor mTLS). El cuerpo
//! del request lo deserializa axum con `axum::Json` usando los tipos del
//! crate `etsi`. La traducción a HTTP de los errores vive en [`super::error_to_response`].

use axum::{
    extract::{Json, Path, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::Value;
use tracing::instrument;
use uuid::Uuid;

use common::ids::SaeId;
use etsi::v014::{Etsi014KeyID, Etsi014KeyIDs, Etsi014KeyRequest};

use crate::{etsi_http::auth::SaePeer, service::DkmsService};

pub fn router(svc: DkmsService) -> Router {
    Router::new()
        .route("/api/v1/keys/:slave_sae/status", get(handle_status))
        .route(
            "/api/v1/keys/:slave_sae/enc_keys",
            get(handle_enc_keys_get).post(handle_enc_keys),
        )
        .route(
            "/api/v1/keys/:master_sae/dec_keys",
            get(handle_dec_keys_get).post(handle_dec_keys),
        )
        .with_state(svc)
}

/// Parámetros URI de `GET …/enc_keys` (§6.2: "number" y/o "size" son los
/// únicos items con los que la spec permite el método GET).
#[derive(Debug, Deserialize)]
struct EncKeysQuery {
    number: Option<u32>,
    size: Option<u32>,
}

impl EncKeysQuery {
    fn into_request(self) -> Etsi014KeyRequest {
        let mut req = Etsi014KeyRequest::default();
        if let Some(n) = self.number {
            req.number = n;
        }
        if let Some(s) = self.size {
            req.size = s;
        }
        req
    }
}

/// Parámetros URI de `GET …/dec_keys` (§6.4: exactamente una `key_ID`,
/// sin extensiones). El nombre y la caja son los de la spec.
#[derive(Debug, Deserialize)]
struct DecKeysQuery {
    #[serde(rename = "key_ID")]
    key_id: Uuid,
}

/// Cabecera fuera de banda `additional-saes` (extensión propia, no ETSI):
/// la aceptan los dos métodos para que GET y POST sean intercambiables.
fn extra_saes_from_headers(headers: &HeaderMap) -> Option<Value> {
    headers
        .get("additional-saes")
        .or_else(|| headers.get("X-Additional-Saes"))
        .and_then(|v| v.to_str().ok())
        .map(|s| Value::String(s.to_owned()))
}

#[instrument(skip(svc, peer))]
async fn handle_status(
    State(svc): State<DkmsService>,
    Path(slave_sae): Path<String>,
    peer: SaePeer,
) -> Response {
    let slave = SaeId::new(slave_sae);
    match svc.status_for(&peer.sae_id, &slave).await {
        Ok(s) => Json(s).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(svc, peer, headers, body))]
async fn handle_enc_keys(
    State(svc): State<DkmsService>,
    Path(slave_sae): Path<String>,
    peer: SaePeer,
    headers: HeaderMap,
    Json(body): Json<Etsi014KeyRequest>,
) -> Response {
    enc_keys_response(svc, slave_sae, peer, &headers, body).await
}

#[instrument(skip(svc, peer, headers))]
async fn handle_enc_keys_get(
    State(svc): State<DkmsService>,
    Path(slave_sae): Path<String>,
    peer: SaePeer,
    headers: HeaderMap,
    Query(q): Query<EncKeysQuery>,
) -> Response {
    enc_keys_response(svc, slave_sae, peer, &headers, q.into_request()).await
}

async fn enc_keys_response(
    svc: DkmsService,
    slave_sae: String,
    peer: SaePeer,
    headers: &HeaderMap,
    body: Etsi014KeyRequest,
) -> Response {
    let slave = SaeId::new(slave_sae);
    let extra_saes_header = extra_saes_from_headers(headers);

    match svc
        .handle_enc_keys(&peer.sae_id, &slave, body, extra_saes_header.as_ref())
        .await
    {
        Ok(c) => Json(c).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[instrument(skip(svc, peer, body))]
async fn handle_dec_keys(
    State(svc): State<DkmsService>,
    Path(master_sae): Path<String>,
    peer: SaePeer,
    Json(body): Json<Etsi014KeyIDs>,
) -> Response {
    dec_keys_response(svc, master_sae, peer, body).await
}

#[instrument(skip(svc, peer))]
async fn handle_dec_keys_get(
    State(svc): State<DkmsService>,
    Path(master_sae): Path<String>,
    peer: SaePeer,
    Query(q): Query<DecKeysQuery>,
) -> Response {
    let body = Etsi014KeyIDs::new(vec![Etsi014KeyID::new(q.key_id)]);
    dec_keys_response(svc, master_sae, peer, body).await
}

async fn dec_keys_response(
    svc: DkmsService,
    master_sae: String,
    peer: SaePeer,
    body: Etsi014KeyIDs,
) -> Response {
    let master = SaeId::new(master_sae);
    match svc.handle_dec_keys(&peer.sae_id, &master, body).await {
        Ok(c) => Json(c).into_response(),
        Err(e) => super::error_to_response(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Uri;

    fn enc_query(uri: &str) -> EncKeysQuery {
        let uri: Uri = uri.parse().unwrap();
        Query::<EncKeysQuery>::try_from_uri(&uri).unwrap().0
    }

    #[test]
    fn get_enc_keys_sin_parametros_usa_los_defaults_del_post() {
        // §6.2: GET sin parámetros equivale a un request vacío — number=1
        // y size = key_size del Status (256), que es exactamente lo que
        // materializan los defaults serde del cuerpo POST.
        let req = enc_query("/api/v1/keys/sae_2/enc_keys").into_request();
        let by_serde: Etsi014KeyRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(req.number, by_serde.number);
        assert_eq!(req.size, by_serde.size);
        assert!(req.additional_slave_sae_ids.is_none());
        assert!(req.extension_mandatory.is_none());
    }

    #[test]
    fn get_enc_keys_number_y_size_por_uri() {
        // Ejemplo literal de la spec: enc_keys?number=3&size=1024.
        let req = enc_query("/x/enc_keys?number=3&size=1024").into_request();
        assert_eq!(req.number, 3);
        assert_eq!(req.size, 1024);
        // Cada parámetro es independiente del otro.
        let req = enc_query("/x/enc_keys?size=512").into_request();
        assert_eq!(req.number, 1);
        assert_eq!(req.size, 512);
    }

    #[test]
    fn get_dec_keys_exige_key_id_con_la_caja_de_la_spec() {
        let uri: Uri = "/x/dec_keys?key_ID=bc490419-7d60-487f-adc1-4ddcc177c139"
            .parse()
            .unwrap();
        let q = Query::<DecKeysQuery>::try_from_uri(&uri).unwrap().0;
        assert_eq!(
            q.key_id,
            "bc490419-7d60-487f-adc1-4ddcc177c139"
                .parse::<Uuid>()
                .unwrap()
        );

        // Sin el parámetro (o con la caja equivocada) el extractor rechaza
        // → 400, como pide la spec para un request mal formado.
        let uri: Uri = "/x/dec_keys?key_id=bc490419-7d60-487f-adc1-4ddcc177c139"
            .parse()
            .unwrap();
        assert!(Query::<DecKeysQuery>::try_from_uri(&uri).is_err());
        let uri: Uri = "/x/dec_keys".parse().unwrap();
        assert!(Query::<DecKeysQuery>::try_from_uri(&uri).is_err());
    }
}
