//! Identidad mTLS del peer en una request HTTP.
//!
//! El listener TLS adjunta un [`PeerIdentity`] a las extensiones de la
//! request justo después del handshake.  Los handlers ETSI lo extraen con
//! [`SaePeer`] o [`DkmsPeer`] según el plano.
//!
//! La identidad útil del SAE/DKMS viaja como **URI** dentro de un *Subject
//! Alternative Name* (SAN) del cert cliente, igual que el Python original.
//! Aceptamos también el *Common Name* como fallback (algunos despliegues
//! antiguos no traen SAN URI).
//!
//! **Edge / proxy mTLS** — cuando el DKMS está detrás de un proxy
//! reverso que termina mTLS (típicamente nginx-ingress en EKS con
//! `auth-tls-pass-certificate-to-upstream: true`), el cert del cliente
//! no aparece en la capa TLS local, sino en el header `ssl-client-cert`
//! (URL-encoded PEM). Si la capa TLS no trae cert, intentamos extraer
//! identidad del header. La confianza viene de la red — solo el proxy
//! reverso puede alcanzar :4001 en cluster, así que aceptar el header
//! sin verificar la firma es coherente con el modelo de despliegue.

use std::sync::Arc;

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, StatusCode},
};
use rustls::pki_types::CertificateDer;
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

use common::ids::{NodeId, SaeId};

/// Header en el que nginx-ingress (con
/// `auth-tls-pass-certificate-to-upstream: true`) deposita el cert del
/// cliente verificado en el borde. Valor: PEM url-encoded.
const SSL_CLIENT_CERT_HEADER: &str = "ssl-client-cert";

/// Información de la otra parte en la conexión TLS — útil para autorizar
/// la request.  Se inyecta a nivel de conexión, **no** de petición.
#[derive(Clone)]
pub struct PeerIdentity {
    /// Cert hoja en DER. `Arc` para clonar sin copiar bytes en cada request.
    pub leaf_der: Option<Arc<Vec<u8>>>,
    /// Primer SAN URI / DNS / CN extraído del cert (best-effort).
    pub san_identifier: Option<String>,
}

impl PeerIdentity {
    /// Construye desde la lista de certs verificados por rustls. Toma el
    /// primero (la hoja) y le saca el SAN.
    pub fn from_verified(certs: Option<&[CertificateDer<'_>]>) -> Self {
        let Some(certs) = certs.and_then(|c| c.first()) else {
            return Self::anonymous();
        };
        let leaf_bytes = certs.as_ref().to_vec();
        let san_identifier = extract_san_identifier(&leaf_bytes);
        Self {
            leaf_der: Some(Arc::new(leaf_bytes)),
            san_identifier,
        }
    }

    pub fn anonymous() -> Self {
        Self {
            leaf_der: None,
            san_identifier: None,
        }
    }

    pub fn is_authenticated(&self) -> bool {
        self.leaf_der.is_some()
    }

    /// Intenta construir una identidad a partir del header
    /// `ssl-client-cert` que nginx-ingress inyecta cuando termina
    /// mTLS en el borde. El valor viene URL-encoded como PEM.
    pub fn from_proxy_header(headers: &HeaderMap) -> Option<Self> {
        let raw = headers.get(SSL_CLIENT_CERT_HEADER)?.to_str().ok()?;
        let pem = decode_proxy_header_value(raw);
        let der = pem_to_der(&pem)?;
        let san_identifier = extract_san_identifier(&der);
        Some(Self {
            leaf_der: Some(Arc::new(der)),
            san_identifier,
        })
    }
}

/// nginx-ingress URL-encoda el PEM (los `\n` aparecen como `%0A`, los
/// espacios como `%20`, etc.). Hacemos la decodificación a mano para no
/// depender de `urlencoding` u otra crate adicional.
fn decode_proxy_header_value(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'%' && i + 2 < bytes.len() {
            let hi = hex_val(bytes[i + 1]);
            let lo = hex_val(bytes[i + 2]);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[inline]
fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(10 + (b - b'a')),
        b'A'..=b'F' => Some(10 + (b - b'A')),
        _ => None,
    }
}

/// Saca el primer bloque PEM (CERTIFICATE) y lo devuelve como DER.
fn pem_to_der(pem: &str) -> Option<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let start = pem.find(BEGIN)? + BEGIN.len();
    let end_off = pem[start..].find(END)?;
    let body: String = pem[start..start + end_off]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    base64_decode(&body).ok()
}

/// Decodificador base64 mínimo (alphabet estándar). Usar este en vez de
/// añadir `base64` al `Cargo.toml` solo para un PEM aislado.
fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    let map = |c: u8| -> Result<u8, ()> {
        match c {
            b'A'..=b'Z' => Ok(c - b'A'),
            b'a'..=b'z' => Ok(26 + (c - b'a')),
            b'0'..=b'9' => Ok(52 + (c - b'0')),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(()),
        }
    };
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut i = 0;
    while i < bytes.len() {
        let a = bytes[i];
        let b = bytes[i + 1];
        let c = bytes[i + 2];
        let d = bytes[i + 3];
        let av = map(a)?;
        let bv = map(b)?;
        out.push((av << 2) | (bv >> 4));
        if c != b'=' {
            let cv = map(c)?;
            out.push(((bv & 0x0F) << 4) | (cv >> 2));
            if d != b'=' {
                let dv = map(d)?;
                out.push(((cv & 0x03) << 6) | dv);
            }
        }
        i += 4;
    }
    Ok(out)
}

/// Extrae el primer SAN útil (URI > DNS > CN) o devuelve `None`.
fn extract_san_identifier(der: &[u8]) -> Option<String> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    // 1) SAN URI
    if let Some(s) = san_first_match(&cert, |gn| matches!(gn, GeneralName::URI(_))) {
        return Some(s);
    }
    // 2) SAN DNS
    if let Some(s) = san_first_match(&cert, |gn| matches!(gn, GeneralName::DNSName(_))) {
        return Some(s);
    }
    // 3) Common Name — materializa la String dentro del scope de `cert`
    //    para no devolver una referencia que sobrevive al cert prestado.
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok().map(str::to_owned));
    cn
}

fn san_first_match<F>(cert: &X509Certificate<'_>, pred: F) -> Option<String>
where
    F: Fn(&GeneralName<'_>) -> bool,
{
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for gn in &san.general_names {
                if pred(gn) {
                    return match gn {
                        GeneralName::URI(u) => Some((*u).to_owned()),
                        GeneralName::DNSName(d) => Some((*d).to_owned()),
                        _ => None,
                    };
                }
            }
        }
    }
    None
}

/// Extractor para handlers servidos en el plano SAE.
///
/// Valida que la conexión venga mTLS-autenticada y devuelve el `SaeId`.
/// Acepta cert presentado en la capa TLS local o, como fallback, el cert
/// inyectado por el proxy reverso vía header `ssl-client-cert`.
pub struct SaePeer {
    pub sae_id: SaeId,
}

#[async_trait]
impl<S> FromRequestParts<S> for SaePeer
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let pid = resolve_peer_identity(&parts.extensions, &parts.headers);
        if !pid.is_authenticated() {
            return Err((StatusCode::UNAUTHORIZED, "missing client certificate"));
        }
        let sae = pid
            .san_identifier
            .as_deref()
            .map(sae_id_from_san)
            .ok_or((StatusCode::UNAUTHORIZED, "no usable SAN in client cert"))?;
        Ok(Self { sae_id: sae })
    }
}

/// Extractor para handlers servidos en el plano DKMS↔DKMS.
pub struct DkmsPeer {
    pub node_id: NodeId,
}

#[async_trait]
impl<S> FromRequestParts<S> for DkmsPeer
where
    S: Send + Sync,
{
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let pid = resolve_peer_identity(&parts.extensions, &parts.headers);
        if !pid.is_authenticated() {
            return Err((StatusCode::UNAUTHORIZED, "missing client certificate"));
        }
        let node = pid
            .san_identifier
            .as_deref()
            .map(node_id_from_san)
            .ok_or((StatusCode::UNAUTHORIZED, "no usable SAN in client cert"))?;
        Ok(Self { node_id: node })
    }
}

/// Resuelve la identidad del peer.
///
/// Prioridad: `ssl-client-cert` (header inyectado por el proxy reverso
/// tras validar mTLS en el borde) sobre la capa TLS local. Esto permite
/// que el cert del SAE original sobreviva el salto proxy→DKMS aunque la
/// capa TLS local termine con un cert distinto (típicamente un
/// "ingress-proxy" cert firmado por la CA interna del sim).
///
/// Si el header no está presente, fallback al cert de la capa TLS
/// (modo "SAE habla directo al DKMS sin proxy intermedio").
///
/// Seguridad: confiamos en el header porque el DKMS:4001 sólo es
/// alcanzable desde dentro del cluster — el único entrypoint público es
/// el proxy reverso (nginx-ingress) que valida `auth-tls-verify-client`
/// contra la CA de simulación antes de fijar el header. Si el modelo de
/// red cambia, hay que reevaluar.
fn resolve_peer_identity(extensions: &axum::http::Extensions, headers: &HeaderMap) -> PeerIdentity {
    if let Some(pid) = PeerIdentity::from_proxy_header(headers) {
        return pid;
    }
    extensions
        .get::<PeerIdentity>()
        .cloned()
        .unwrap_or_else(PeerIdentity::anonymous)
}

/// Convierte un SAN URI a `SaeId` normalizado.
///
/// Acepta los prefijos típicos:
/// * `sae://organisation/sae-x` (legacy Python original)
/// * `urn:dkms:sae:sae-x` (el que emite `orchestrator/sae_certificates.py`)
///
/// Si el SAN no parece URI, se devuelve tal cual: muchos despliegues usan
/// directamente el SAE id como CN o DNS.
fn sae_id_from_san(s: &str) -> SaeId {
    SaeId::new(strip_known_sae_prefix(s).to_owned())
}

fn node_id_from_san(s: &str) -> NodeId {
    NodeId::new(strip_known_dkms_prefix(s).to_owned())
}

fn strip_known_sae_prefix(s: &str) -> &str {
    for prefix in ["urn:dkms:sae:", "sae://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            // `sae://organisation/sae-x` → quedarse con el último segmento.
            if prefix == "sae://" {
                return rest.rsplit('/').next().unwrap_or(rest);
            }
            return rest;
        }
    }
    s
}

fn strip_known_dkms_prefix(s: &str) -> &str {
    for prefix in ["urn:dkms:node:", "dkms://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            if prefix == "dkms://" {
                return rest.rsplit('/').next().unwrap_or(rest);
            }
            return rest;
        }
    }
    s
}
