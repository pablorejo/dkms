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

use std::sync::Arc;

use axum::{
    async_trait,
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use rustls::pki_types::CertificateDer;
use x509_parser::prelude::{
    FromDer, GeneralName, ParsedExtension, X509Certificate,
};

use common::ids::{NodeId, SaeId};

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
        let pid = parts
            .extensions
            .get::<PeerIdentity>()
            .cloned()
            .unwrap_or_else(PeerIdentity::anonymous);
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
        let pid = parts
            .extensions
            .get::<PeerIdentity>()
            .cloned()
            .unwrap_or_else(PeerIdentity::anonymous);
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

/// Convierte un SAN URI (p. ej. `sae://organisation/sae-x`) a `SaeId`.
///
/// Si el SAN no parece URI, se devuelve tal cual: muchos despliegues usan
/// directamente el SAE id como CN o DNS.
fn sae_id_from_san(s: &str) -> SaeId {
    SaeId::new(strip_known_scheme(s, "sae://").to_owned())
}

fn node_id_from_san(s: &str) -> NodeId {
    NodeId::new(strip_known_scheme(s, "dkms://").to_owned())
}

fn strip_known_scheme<'a>(s: &'a str, prefix: &str) -> &'a str {
    s.strip_prefix(prefix).unwrap_or(s)
}
