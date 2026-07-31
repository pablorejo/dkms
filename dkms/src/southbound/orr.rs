//! Cliente gRPC del DKMS contra el ORR co-localizado.
//!
//! Sirve para empujar **mensajes** (claves QKD entre buffers) por el
//! transporte ORR↔QKC en lugar del HTTP/2 ETSI 020. Modelo:
//!
//! * `body` del mensaje = bytes crudos de la clave (lo que entrará en
//!   el buffer DEC del DKMS destino).
//! * `header_dkms` = metadatos de la clave (`key_id`, `sae_origin`,
//!   `sae_destination`, `key_size_bits`, `request_id`, `flow_id`,
//!   `timestamp`...). Viaja en cleartext **a través del ORR y QKC** —
//!   el ORR no lo cifra; solo cifra el body con ML-KEM+XOR si el modo
//!   onion lo pide. El QKC OTP-cifra el `payload` por enlace.
//!
//! Este cliente no toca el flujo actual ETSI 020 sobre HTTP/2; está
//! pensado para que un futuro cambio en `peer_client.rs` o `service.rs`
//! lo enchufe como transporte alternativo.

use std::time::Duration;

use anyhow::anyhow;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::Request;
use tracing::debug;

use common::proto::{
    common::v1::NodeId as ProtoNodeId,
    orr::v1::{
        orr_control_client::OrrControlClient, DeliveredMessage, SendMessageRequest,
        SendMessageResponse, StreamDeliveriesRequest,
    },
};
use tonic::Streaming;

use crate::{
    config::SouthboundCfg,
    error::{DkmsError, Result},
};

// ─── Claves estándar para el header_dkms ───────────────────────────
//
// El proto `SendMessageRequest.app_header` es `map<string, string>`. El
// ORR lo serializa msgpack en `frame.header_dkms_mp` y el ORR destino
// lo entrega al DKMS destino en `DeliveredMessage.app_header`. Estas
// son las claves canónicas que el DKMS pone/lee — quienquiera que
// modifique este contrato debe actualizar ambos extremos.

/// UUID que identifica la clave en los buffers (ENC origen ↔ DEC dest).
/// Identifica el tipo de mensaje DKMS sobre transporte ORR.
/// Valores: `"DKMS_BUFFER"` (fill de buffers compartidos entre DKMSs,
/// gestionado por el Generator) o ausente / `"ETSI020"` para el flujo
/// de claves SAE clásico.
pub const HDR_MSG_TYPE: &str = "msg_type";
/// Para mensajes `DKMS_BUFFER`: TCP `host:port` donde el peer destino
/// debe enviar el FRAME_ACK que mueve la clave de `ack_pending` a
/// `buffer_enc`. Solo informativo para `ETSI020`.
pub const HDR_ACK_ENDPOINT: &str = "ack_endpoint";

pub const MSG_TYPE_DKMS_BUFFER: &str = "DKMS_BUFFER";
pub const MSG_TYPE_ETSI020: &str = "ETSI020";

pub const HDR_KEY_ID: &str = "key_id";
/// SAE que pidió originar la clave (ETSI 014).
pub const HDR_SAE_ORIGIN: &str = "sae_origin";
/// SAE destinatario de la clave.
pub const HDR_SAE_DESTINATION: &str = "sae_destination";
/// Tamaño de la clave en bits (string decimal, ej. "256").
pub const HDR_KEY_SIZE_BITS: &str = "key_size_bits";
/// Huella de la clave que viaja: `hex(SHA-256(key_id ‖ bytes)[..16])`.
///
/// El receptor la comprueba ANTES de guardar la clave y de acusar recibo.
/// Es la única verificación de integridad de todo el camino: el OTP del
/// enlace QKC no lleva MAC, así que cualquier corrupción por debajo
/// —secretos de época desincronizados tras un reinicio, un bit cambiado—
/// llegaba aquí como material "válido", se guardaba, se acusaba recibo, y
/// acababa haciendo que los dos SAEs de una petición ETSI-014 obtuvieran
/// claves DISTINTAS sin un solo error.
///
/// Publicar la huella no debilita la clave: son 256 bits de entropía, así
/// que la preimagen no es atacable, y va ligada al `key_id` para que no
/// pueda reutilizarse en otra entrada.
pub const HDR_KEY_DIGEST: &str = "key_digest";

/// `request_id` de la petición ETSI 020 que disparó este envío.
pub const HDR_REQUEST_ID: &str = "request_id";
/// Flow id opcional (si aplica routing por flujo).
pub const HDR_FLOW_ID: &str = "flow_id";
/// Unix milliseconds de emisión.
pub const HDR_TIMESTAMP_MS: &str = "timestamp_ms";

/// Calcula el valor de [`HDR_KEY_DIGEST`] para una clave.
pub fn key_digest(key_id: &str, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(key_id.as_bytes());
    h.update(bytes);
    hex::encode(&h.finalize()[..16])
}

#[derive(Clone)]
pub struct OrrClient {
    channel: Channel,
    rpc_timeout: Duration,
}

impl OrrClient {
    /// Conecta al ORR co-localizado. El endpoint sale de
    /// `SouthboundCfg::orr_endpoint`. Si está vacío, devuelve `Ok(None)`
    /// — el DKMS sigue funcionando con su transporte HTTP/2 actual.
    pub async fn connect_opt(
        cfg: &SouthboundCfg,
        tls: Option<ClientTlsConfig>,
    ) -> Result<Option<Self>> {
        let Some(ep) = cfg.orr_endpoint.as_deref() else {
            return Ok(None);
        };
        if ep.is_empty() {
            return Ok(None);
        }
        let mut endpoint = Endpoint::from_shared(ep.to_string())
            .map_err(|e| DkmsError::Other(anyhow!("orr endpoint inválido: {e}")))?
            .connect_timeout(Duration::from_millis(cfg.connect_timeout_ms))
            .timeout(Duration::from_millis(cfg.rpc_timeout_ms));

        if let Some(t) = tls {
            endpoint = endpoint
                .tls_config(t)
                .map_err(|e| DkmsError::Other(anyhow!("orr tls: {e}")))?;
        }

        let channel = endpoint
            .connect()
            .await
            .map_err(|e| DkmsError::Other(anyhow!("orr connect: {e}")))?;

        debug!(endpoint = ep, "orr client connected");
        Ok(Some(Self {
            channel,
            rpc_timeout: Duration::from_millis(cfg.rpc_timeout_ms),
        }))
    }

    /// Manda un mensaje al ORR destino con el body y el header_dkms.
    ///
    /// * `dest_orr_id`: id lógico del ORR destino (el ORR resuelve a
    ///   `qkc_id` por su tabla de peers).
    /// * `body`: bytes crudos de la clave QKD (lo que entrará en
    ///   `dec[key_id]` en el DKMS destino).
    /// * `header_dkms`: `BTreeMap<String, String>` con las claves
    ///   canónicas (`HDR_*`). Va al `app_header` del proto.
    /// * `max_hops`: routing mode (0 = passthrough, 1 = PQC E2E,
    ///   -1 = onion completo, N >= 2 = onion truncado con N hops
    ///   aleatorios elegidos por el ORR origen).
    pub async fn send_key(
        &self,
        dest_orr_id: &str,
        body: Vec<u8>,
        header_dkms: std::collections::BTreeMap<String, String>,
        max_hops: i32,
        grade: u8,
    ) -> Result<SendMessageResponse> {
        let mut client = OrrControlClient::new(self.channel.clone());
        let req = SendMessageRequest {
            destination: Some(ProtoNodeId {
                value: dest_orr_id.into(),
            }),
            payload: body,
            max_hops,
            has_max_hops: true,
            app_header: header_dkms.into_iter().collect(),
            fire_and_forget: false,
            grade: u32::from(grade),
        };
        let mut request = Request::new(req);
        request.set_timeout(self.rpc_timeout);
        let resp = client
            .send_message(request)
            .await
            .map_err(|s| DkmsError::Other(anyhow!("orr send_message: {s}")))?;
        Ok(resp.into_inner())
    }

    /// Abre el stream `OrrControl::StreamDeliveries`. El stream no
    /// lleva timeout RPC porque es long-lived; cualquier corte se ve
    /// como `Err(Status)` en `next()` y el caller decide reconectar.
    pub async fn subscribe_deliveries(
        &self,
        subscriber_id: impl Into<String>,
    ) -> Result<Streaming<DeliveredMessage>> {
        let mut client = OrrControlClient::new(self.channel.clone());
        let req = Request::new(StreamDeliveriesRequest {
            subscriber_id: subscriber_id.into(),
        });
        let stream = client
            .stream_deliveries(req)
            .await
            .map_err(|s| DkmsError::Other(anyhow!("orr stream_deliveries: {s}")))?;
        Ok(stream.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_keys_are_stable() {
        // Cambiar estos breaks la wire con cualquier consumidor que ya
        // exista — si cambias una, actualiza el otro extremo a la vez.
        assert_eq!(HDR_KEY_ID, "key_id");
        assert_eq!(HDR_SAE_ORIGIN, "sae_origin");
        assert_eq!(HDR_SAE_DESTINATION, "sae_destination");
        assert_eq!(HDR_KEY_SIZE_BITS, "key_size_bits");
        assert_eq!(HDR_REQUEST_ID, "request_id");
        assert_eq!(HDR_FLOW_ID, "flow_id");
        assert_eq!(HDR_TIMESTAMP_MS, "timestamp_ms");
    }
}

#[cfg(test)]
mod digest_tests {
    use super::*;

    #[test]
    fn digest_binds_key_id_and_bytes() {
        let a = key_digest("id-1", &[0xABu8; 32]);
        assert_eq!(a, key_digest("id-1", &[0xABu8; 32]), "determinista");
        assert_ne!(a, key_digest("id-2", &[0xABu8; 32]), "ligado al key_id");
        let mut otros = [0xABu8; 32];
        otros[31] ^= 1;
        assert_ne!(a, key_digest("id-1", &otros), "un bit cambiado se detecta");
        assert_eq!(a.len(), 32, "16 bytes en hex");
    }
}
