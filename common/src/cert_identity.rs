//! Identidad de nodo a partir de certificados X.509.
//!
//! Los certificados de nodo que emite `docker/gen-certs.sh` llevan la
//! identidad en el SAN: `URI:dkms://<node_id>` (`orr_1`, `dkms-1`, `qkc-1`),
//! firmados por la CA de red (`net-ca`). Todo lo que necesite saber *quién*
//! es el otro extremo a partir de un cert pasa por aquí: el SDN para los
//! anuncios, el ORR para atar el `from` de `EstablishSecret` al cert mTLS y
//! para verificar la firma del anuncio de pubkey, y las pruebas.

use rustls::pki_types::{CertificateDer, UnixTime};
use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509Certificate};

/// Esquema del SAN URI de los certs de nodo.
pub const NODE_URI_SCHEME: &str = "dkms://";

/// Extrae el primer SAN útil (URI > DNS > CN) de un cert DER.
pub fn extract_san_identifier(der: &[u8]) -> Option<String> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    for want_uri in [true, false] {
        for ext in cert.extensions() {
            if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
                for gn in &san.general_names {
                    match gn {
                        GeneralName::URI(u) if want_uri => return Some((*u).to_owned()),
                        GeneralName::DNSName(d) if !want_uri => return Some((*d).to_owned()),
                        _ => {}
                    }
                }
            }
        }
    }
    // Materializar la String dentro del scope de `cert`: devolver el iterador
    // encadenado dejaría un temporal que sobrevive al cert prestado (E0597).
    let cn = cert
        .subject()
        .iter_common_name()
        .next()
        .and_then(|cn| cn.as_str().ok().map(str::to_owned));
    cn
}

/// `node_id` de un cert de nodo: el SAN `URI:dkms://<id>`, en minúsculas.
/// `None` si el cert no lleva ese SAN (un cert de SAE, por ejemplo).
pub fn node_id_from_cert(der: &[u8]) -> Option<String> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    for ext in cert.extensions() {
        if let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() {
            for gn in &san.general_names {
                if let GeneralName::URI(u) = gn {
                    if let Some(id) = u.strip_prefix(NODE_URI_SCHEME) {
                        if !id.is_empty() {
                            return Some(id.to_ascii_lowercase());
                        }
                    }
                }
            }
        }
    }
    None
}

/// [`node_id_from_cert`] de la hoja de una cadena verificada por TLS.
pub fn node_id_from_certs(certs: &[CertificateDer<'_>]) -> Option<String> {
    certs.first().and_then(|c| node_id_from_cert(c.as_ref()))
}

/// Clave pública en bruto del SPKI (el BIT STRING sin el byte de padding).
/// Para ML-DSA es la clave de verificación tal cual la codifica FIPS 204.
pub fn spki_public_key(der: &[u8]) -> Option<Vec<u8>> {
    let (_, cert) = X509Certificate::from_der(der).ok()?;
    Some(cert.public_key().subject_public_key.data.to_vec())
}

/// Raíces de confianza (la CA de red, o un bundle).
#[derive(Clone, Debug)]
pub struct TrustRoots(Vec<CertificateDer<'static>>);

impl TrustRoots {
    pub fn from_pem(pem: &[u8]) -> Result<Self, String> {
        use rustls::pki_types::pem::PemObject;
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(pem)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("CA PEM: {e}"))?;
        if certs.is_empty() {
            return Err("CA PEM sin certificados".to_string());
        }
        Ok(Self(certs))
    }

    pub fn from_file(path: &std::path::Path) -> Result<Self, String> {
        let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_pem(&pem)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Verifica que `chain[0]` (DER, hoja) encadena hasta una de las `roots`
/// —con `chain[1..]` como intermedios—, es válido ahora, sirve para
/// autenticar como cliente y su SAN dice `dkms://<expected_id>`. Devuelve la
/// clave pública en bruto de la hoja, lista para verificar firmas hechas
/// con la clave de ese certificado.
///
/// Es lo que hace que un pin sobreviva a los reinicios: el ancla es la
/// identidad del certificado (que ya existe y ya se rota), no el proceso.
pub fn verify_node_cert(
    chain: &[Vec<u8>],
    roots: &TrustRoots,
    expected_id: &str,
) -> Result<Vec<u8>, String> {
    let Some(leaf_der) = chain.first() else {
        return Err("cadena vacía".to_string());
    };
    let leaf = CertificateDer::from(leaf_der.as_slice());
    let ee = webpki::EndEntityCert::try_from(&leaf).map_err(|e| format!("hoja: {e}"))?;
    let anchors = roots
        .0
        .iter()
        .map(webpki::anchor_from_trusted_cert)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("CA: {e}"))?;
    let intermediates: Vec<CertificateDer<'_>> = chain[1..]
        .iter()
        .map(|c| CertificateDer::from(c.as_slice()))
        .collect();
    ee.verify_for_usage(
        crate::tls_pqc::verify_algs(),
        &anchors,
        &intermediates,
        UnixTime::now(),
        webpki::KeyUsage::client_auth(),
        None,
        None,
    )
    .map_err(|e| format!("la cadena no verifica contra la CA de red: {e}"))?;
    let id = node_id_from_cert(leaf_der)
        .ok_or_else(|| "el cert no lleva SAN URI dkms://<id>".to_string())?;
    if id != expected_id.to_ascii_lowercase() {
        return Err(format!(
            "el cert es de `{id}`, no de `{}`",
            expected_id.to_ascii_lowercase()
        ));
    }
    spki_public_key(leaf_der).ok_or_else(|| "SPKI ilegible".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::pqc_sign::{sign_orr_pubkey_with, verify_orr_pubkey, MlDsa65Signer};

    /// Con la PKI ML-DSA de prueba (openssl ≥ 3.5): el SAN da el id, la
    /// cadena verifica contra su CA y no contra otra, el id esperado se
    /// comprueba, y la clave del SPKI verifica lo que firma la clave privada
    /// del mismo cert — que es todo lo que necesita el anuncio del ORR.
    #[test]
    fn a_node_cert_identifies_its_node_and_signs_verifiably_needs_openssl35() {
        let dir = std::env::temp_dir().join(format!("cert_identity_{}", std::process::id()));
        let Some(pki) = crate::test_support::mldsa_test_pki(&dir, &["orr_1", "orr_2"]) else {
            crate::test_support::skip_or_fail("openssl sin ML-DSA (<3.5)");
            return;
        };
        let leaf = pki.cert_der("orr_1");
        assert_eq!(node_id_from_cert(&leaf).as_deref(), Some("orr_1"));
        assert_eq!(
            extract_san_identifier(&leaf).as_deref(),
            Some("dkms://orr_1")
        );

        let roots = TrustRoots::from_file(&pki.ca_crt).unwrap();
        let vk =
            verify_node_cert(std::slice::from_ref(&leaf), &roots, "ORR_1").expect("cadena + SAN");
        assert_eq!(vk.len(), crate::crypto::pqc_sign::VERIFYING_KEY_LEN);
        assert!(
            verify_node_cert(std::slice::from_ref(&leaf), &roots, "orr_2").is_err(),
            "otro id"
        );
        assert!(verify_node_cert(&[pki.cert_der("orr_2")], &roots, "orr_1").is_err());

        // Una CA ajena no vale aunque el cert sea perfecto.
        let rogue =
            std::env::temp_dir().join(format!("cert_identity_rogue_{}", std::process::id()));
        let rogue_pki = crate::test_support::mldsa_test_pki(&rogue, &["orr_1"]).unwrap();
        let rogue_roots = TrustRoots::from_file(&rogue_pki.ca_crt).unwrap();
        assert!(verify_node_cert(std::slice::from_ref(&leaf), &rogue_roots, "orr_1").is_err());
        assert!(verify_node_cert(&[rogue_pki.cert_der("orr_1")], &roots, "orr_1").is_err());

        // La clave privada del cert firma; el SPKI del cert verifica.
        let signer = MlDsa65Signer::from_pkcs8_pem(&std::fs::read(pki.key("orr_1")).unwrap())
            .expect("clave ML-DSA-65 del cert");
        let pk = vec![0x42u8; 1184];
        let sig = sign_orr_pubkey_with(&signer, "orr_1", "ml-kem-768", &pk);
        verify_orr_pubkey(&vk, "orr_1", "ml-kem-768", &pk, &sig).expect("firma del cert");
        assert!(verify_orr_pubkey(&vk, "orr_1", "ml-kem-768", &[0x43u8; 1184], &sig).is_err());
        assert_eq!(
            signer.verifying_key(),
            vk,
            "SPKI == clave de verificación ML-DSA"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rogue);
    }
}
