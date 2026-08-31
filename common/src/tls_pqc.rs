//! TLS con **firma post-cuántica ML-DSA** en los certificados (FIPS 204).
//!
//! rustls verifica ML-DSA de fábrica (rustls-webpki expone `ML_DSA_65`), pero
//! (a) no lo lista en el provider por defecto y (b) no sabe **cargar** una
//! clave privada ML-DSA para firmar el handshake (su módulo `pq` es solo
//! ML-KEM). Este módulo aporta las dos piezas:
//!
//!   * `MlDsa65SigningKey` — implementa `rustls::sign::SigningKey`/`Signer`
//!     sobre la crate `ml-dsa` (firma estándar, contexto vacío, como pide
//!     draft-tls-mldsa; interopera con la verificación de aws-lc-rs).
//!   * `pqc_crypto_provider()` — clona el provider aws-lc-rs por defecto y le
//!     añade ML-DSA tanto en la verificación (`signature_verification_algorithms`)
//!     como en la carga de claves (`key_provider`, con fallback a RSA/ECDSA/EdDSA).
//!
//! Los certs ML-DSA se generan con openssl 3.5+ (`req -newkey ML-DSA-65`).

use std::sync::Arc;

use ml_dsa::pkcs8::DecodePrivateKey;
use ml_dsa::signature::Signer as _;
use ml_dsa::{MlDsa65, SigningKey as MlDsaSigningKey};
use rustls::crypto::{aws_lc_rs, CryptoProvider, KeyProvider};
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{Error, SignatureAlgorithm, SignatureScheme};

/// Clave de firma ML-DSA-65 que rustls puede usar para firmar el handshake.
/// Se carga desde la forma **seed-only** del PKCS8 (la que emite openssl con
/// `-provparam ml-dsa.output_formats=seed-only`, y que produce `gen-certs.sh`).
#[derive(Debug)]
struct MlDsa65Key {
    inner: Arc<MlDsaSigningKey<MlDsa65>>,
}

impl MlDsa65Key {
    /// Intenta cargar una clave ML-DSA-65 (forma semilla) desde su PKCS8 DER.
    /// `None` si no es una clave ML-DSA, para que el caller pruebe otros tipos
    /// (RSA/ECDSA/EdDSA).
    fn from_pkcs8_der(der: &[u8]) -> Option<Self> {
        MlDsaSigningKey::<MlDsa65>::from_pkcs8_der(der)
            .ok()
            .map(|sk| Self {
                inner: Arc::new(sk),
            })
    }
}

impl SigningKey for MlDsa65Key {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered.contains(&SignatureScheme::ML_DSA_65).then(|| {
            Box::new(MlDsa65Signer {
                inner: self.inner.clone(),
            }) as Box<dyn Signer>
        })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        // pki-types aún no tiene una variante ML-DSA; el matching efectivo lo
        // hace `choose_scheme` + la verificación webpki. Unknown(0x0905) refleja
        // el code point TLS de ML_DSA_65 sin colisionar con los clásicos.
        SignatureAlgorithm::Unknown(0x09)
    }
}

#[derive(Debug)]
struct MlDsa65Signer {
    inner: Arc<MlDsaSigningKey<MlDsa65>>,
}

impl Signer for MlDsa65Signer {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        // `Signer::sign` de la crate = ML-DSA.Sign estándar con contexto vacío,
        // que es lo que espera TLS 1.3.
        Ok(self.inner.sign(message).encode().to_vec())
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ML_DSA_65
    }
}

/// `KeyProvider` que carga claves ML-DSA-65 y, si no lo son, delega en el
/// provider aws-lc-rs por defecto (RSA/ECDSA/EdDSA). Así conviven certs
/// clásicos y post-cuánticos durante la migración.
#[derive(Debug)]
struct PqcKeyProvider;

impl KeyProvider for PqcKeyProvider {
    fn load_private_key(
        &self,
        key_der: PrivateKeyDer<'static>,
    ) -> Result<Arc<dyn SigningKey>, Error> {
        if let PrivateKeyDer::Pkcs8(pkcs8) = &key_der {
            if let Some(k) = MlDsa65Key::from_pkcs8_der(pkcs8.secret_pkcs8_der()) {
                // Una línea por clave cargada: en campaña confirma que el nodo
                // está firmando con PQC y no cayó al camino clásico.
                tracing::info!("tls_pqc: clave de firma ML-DSA-65 cargada (firma PQC activa)");
                return Ok(Arc::new(k));
            }
        }
        tracing::debug!(
            "tls_pqc: clave no-ML-DSA; delego en el provider clásico (RSA/ECDSA/EdDSA)"
        );
        aws_lc_rs::default_provider()
            .key_provider
            .load_private_key(key_der)
    }
}

/// Algoritmos de verificación por DEFECTO: **solo ML-DSA-65**. Desde el
/// 2026-08-31 la autenticación sigue la misma regla que el intercambio de
/// claves ("sin respaldo clásico en ningún plano"): un cert RSA/ECDSA firmado
/// por la misma CA autenticaba igual, y eso dejaba la *identidad* rompible
/// clásicamente aunque el KX fuese híbrido. Lo clásico queda detrás del
/// opt-in explícito de migración [`classical_certs_allowed`].
static VERIFY_ALGS_PQC: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm] =
    &[webpki::aws_lc_rs::ML_DSA_65];

/// Tabla de MIGRACIÓN: los clásicos del provider aws-lc-rs además de
/// ML-DSA-65. Solo activa con `DKMS_TLS_ACCEPT_CLASSICAL_CERTS=1`.
static VERIFY_ALGS_CLASSICAL: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm] = &[
    webpki::aws_lc_rs::ML_DSA_65,
    webpki::aws_lc_rs::ECDSA_P256_SHA256,
    webpki::aws_lc_rs::ECDSA_P384_SHA384,
    webpki::aws_lc_rs::ED25519,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512,
    webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
];

#[allow(clippy::type_complexity)]
static VERIFY_MAPPING_PQC: &[(
    SignatureScheme,
    &[&dyn rustls::pki_types::SignatureVerificationAlgorithm],
)] = &[(SignatureScheme::ML_DSA_65, &[webpki::aws_lc_rs::ML_DSA_65])];

#[allow(clippy::type_complexity)]
static VERIFY_MAPPING_CLASSICAL: &[(
    SignatureScheme,
    &[&dyn rustls::pki_types::SignatureVerificationAlgorithm],
)] = &[
    (SignatureScheme::ML_DSA_65, &[webpki::aws_lc_rs::ML_DSA_65]),
    (
        SignatureScheme::ECDSA_NISTP256_SHA256,
        &[webpki::aws_lc_rs::ECDSA_P256_SHA256],
    ),
    (
        SignatureScheme::ECDSA_NISTP384_SHA384,
        &[webpki::aws_lc_rs::ECDSA_P384_SHA384],
    ),
    (SignatureScheme::ED25519, &[webpki::aws_lc_rs::ED25519]),
    (
        SignatureScheme::RSA_PKCS1_SHA256,
        &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256],
    ),
    (
        SignatureScheme::RSA_PKCS1_SHA384,
        &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384],
    ),
    (
        SignatureScheme::RSA_PKCS1_SHA512,
        &[webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512],
    ),
    (
        SignatureScheme::RSA_PSS_SHA256,
        &[webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY],
    ),
];

const VERIFY_SUPPORTED_PQC: rustls::crypto::WebPkiSupportedAlgorithms =
    rustls::crypto::WebPkiSupportedAlgorithms {
        all: VERIFY_ALGS_PQC,
        mapping: VERIFY_MAPPING_PQC,
    };

const VERIFY_SUPPORTED_CLASSICAL: rustls::crypto::WebPkiSupportedAlgorithms =
    rustls::crypto::WebPkiSupportedAlgorithms {
        all: VERIFY_ALGS_CLASSICAL,
        mapping: VERIFY_MAPPING_CLASSICAL,
    };

/// ¿Está activo el opt-in de migración que acepta certificados clásicos
/// (RSA/ECDSA/Ed25519) además de ML-DSA? `DKMS_TLS_ACCEPT_CLASSICAL_CERTS=1`.
/// Se lee UNA vez (el provider se instala una vez por proceso) y avisa alto:
/// con él puesto la autenticación TLS deja de ser post-cuántica.
pub fn classical_certs_allowed() -> bool {
    static ALLOWED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ALLOWED.get_or_init(|| {
        let on = std::env::var("DKMS_TLS_ACCEPT_CLASSICAL_CERTS")
            .map(|v| v == "1")
            .unwrap_or(false);
        if on {
            tracing::warn!(
                "tls_pqc: DKMS_TLS_ACCEPT_CLASSICAL_CERTS=1 — se aceptan certificados \
                 RSA/ECDSA: la AUTENTICACIÓN TLS deja de ser post-cuántica (solo migración)"
            );
        }
        on
    })
}

fn verify_supported_for(classical: bool) -> &'static rustls::crypto::WebPkiSupportedAlgorithms {
    if classical {
        &VERIFY_SUPPORTED_CLASSICAL
    } else {
        &VERIFY_SUPPORTED_PQC
    }
}

fn verify_supported() -> &'static rustls::crypto::WebPkiSupportedAlgorithms {
    verify_supported_for(classical_certs_allowed())
}

/// La lista de algoritmos de verificación vigente (para `webpki` fuera de
/// rustls, p. ej. la verificación de anuncios firmados de `cert_identity`).
pub(crate) fn verify_algs(
) -> &'static [&'static dyn rustls::pki_types::SignatureVerificationAlgorithm] {
    verify_supported().all
}

static PQC_KEY_PROVIDER: PqcKeyProvider = PqcKeyProvider;

/// Construye el `CryptoProvider` con firma ML-DSA. Base:
/// `aws_lc_rs::default_provider()`. La política de certs clásicos la decide
/// [`classical_certs_allowed`]; `build_provider_with` es la costura de test.
fn build_provider() -> CryptoProvider {
    build_provider_with(classical_certs_allowed())
}

fn build_provider_with(classical: bool) -> CryptoProvider {
    let mut provider = aws_lc_rs::default_provider();
    provider.signature_verification_algorithms = *verify_supported_for(classical);
    provider.key_provider = &PQC_KEY_PROVIDER;

    // Intercambio de claves HÍBRIDO post-cuántico, y SOLO ese.
    //
    // Autenticar con ML-DSA protege de que alguien se haga pasar por un nodo,
    // pero no de «grabar ahora, descifrar después»: si el secreto de sesión se
    // acuerda solo con X25519, un adversario con ordenador cuántico puede
    // guardar el tráfico de hoy y descifrarlo mañana. Y por este TLS viajan
    // claves de sesión de SAE.
    //
    // rustls trae `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem) pero solo lo
    // ofrece por defecto con la feature `prefer-post-quantum`, que no está
    // activa aquí — así que se pone explícitamente. Es híbrido: el secreto
    // sale de combinar X25519 **y** ML-KEM-768, de modo que sigue siendo tan
    // seguro como X25519 aunque ML-KEM fallara, y resistente a cuántico
    // aunque X25519 caiga.
    //
    // Hasta 2026-08-30 los grupos clásicos quedaban detrás como respaldo
    // «para no romper a un peer que aún no lo soporte». Eso convertía la
    // garantía en una preferencia: bastaba un cliente que no ofreciera el
    // híbrido para que la sesión se negociara con X25519 a secas, sin que
    // nada lo dijera. Desde entonces no hay respaldo en ningún plano: un
    // peer o un SAE sin X25519MLKEM768 (OpenSSL < 3.5) no conecta, y lo ve
    // como `HandshakeFailure` (ver docker/README.md, requisitos del cliente).
    provider.kx_groups = vec![aws_lc_rs::kx_group::X25519MLKEM768];
    provider
}

/// El único grupo de intercambio de claves que este código negocia.
pub const REQUIRED_KX_GROUP: rustls::NamedGroup = rustls::NamedGroup::X25519MLKEM768;

/// Provider criptográfico con firma ML-DSA en certificados (los clásicos
/// solo con el opt-in de migración, ver [`classical_certs_allowed`]).
pub fn pqc_crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(build_provider())
}

/// Verificador de servidor que acepta cualquier certificado. SOLO para el
/// self-check de arranque, donde el nodo habla consigo mismo y lo que se
/// comprueba es la negociación, no la confianza.
#[derive(Debug)]
struct TrustAnything;

impl rustls::client::danger::ServerCertVerifier for TrustAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, verify_supported())
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, verify_supported())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        verify_supported().supported_schemes()
    }
}

fn feed<D>(dst: &mut rustls::ConnectionCommon<D>, mut data: &[u8]) -> Result<(), String> {
    while !data.is_empty() {
        let n = dst.read_tls(&mut data).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        dst.process_new_packets().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Handshake TLS completo en memoria entre `server` y `client`. Devuelve el
/// grupo de intercambio de claves negociado. Los flights ML-DSA son grandes
/// (cert ~4 KB + CertVerify ~3.3 KB), así que cada lado se drena entero.
pub fn handshake_in_memory(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
) -> Result<rustls::NamedGroup, String> {
    let mut srv = rustls::ServerConnection::new(server).map_err(|e| e.to_string())?;
    let name = rustls::pki_types::ServerName::try_from("localhost").map_err(|e| e.to_string())?;
    let mut cli = rustls::ClientConnection::new(client, name).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    for _ in 0..30 {
        let mut progressed = false;
        buf.clear();
        while cli.wants_write() {
            cli.write_tls(&mut buf).map_err(|e| e.to_string())?;
            progressed = true;
        }
        feed(&mut srv, &buf)?;
        buf.clear();
        while srv.wants_write() {
            srv.write_tls(&mut buf).map_err(|e| e.to_string())?;
            progressed = true;
        }
        feed(&mut cli, &buf)?;
        if !cli.is_handshaking() && !srv.is_handshaking() {
            break;
        }
        if !progressed {
            break;
        }
    }
    if cli.is_handshaking() || srv.is_handshaking() {
        return Err("el handshake no llegó a completarse".to_string());
    }
    srv.negotiated_key_exchange_group()
        .map(|g| g.name())
        .ok_or_else(|| "sin grupo de intercambio negociado".to_string())
}

/// Self-check de arranque: con la identidad TLS del nodo (`cert`/`key` en
/// PEM) se hace un handshake consigo mismo y se comprueba que lo que se
/// negocia es [`REQUIRED_KX_GROUP`]. Configurarlo no basta —hay que ver que
/// se elige— y es mejor abortar aquí, con la causa, que descubrirlo cuando
/// un peer no conecte.
pub fn self_check_hybrid_kx(cert_pem: &[u8], key_pem: &[u8]) -> Result<(), String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("cert PEM: {e}"))?;
    if certs.is_empty() {
        return Err("cert PEM sin certificados".to_string());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|e| format!("key PEM: {e}"))?;
    let provider = pqc_crypto_provider();
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("identidad del nodo: {e}"))?;
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(TrustAnything))
        .with_no_client_auth();
    let kx = handshake_in_memory(Arc::new(server), Arc::new(client))?;
    if kx != REQUIRED_KX_GROUP {
        return Err(format!(
            "el self-check negoció {kx:?} en vez de {REQUIRED_KX_GROUP:?}: este binario no \
             es post-cuántico en el intercambio de claves"
        ));
    }
    tracing::info!(
        kx = ?kx,
        "tls_pqc: self-check OK — el intercambio de claves TLS es el híbrido post-cuántico"
    );
    Ok(())
}

/// [`self_check_hybrid_kx`] leyendo la identidad de disco.
pub fn self_check_hybrid_kx_files(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<(), String> {
    let cert = std::fs::read(cert_path).map_err(|e| format!("{}: {e}", cert_path.display()))?;
    let key = std::fs::read(key_path).map_err(|e| format!("{}: {e}", key_path.display()))?;
    self_check_hybrid_kx(&cert, &key)
}

/// Instala el provider PQC como **default del proceso**. Debe llamarse una vez
/// al arrancar cada binario **antes** de cualquier handshake TLS. Es lo que
/// hace que los clientes que usan el provider por defecto —reqwest
/// (`CryptoProvider::get_default()`) y tonic— puedan cargar/verificar certs
/// ML-DSA, no solo el `common::tls` que ya lo pasa explícito. Idempotente:
/// devuelve `true` si lo instaló, `false` si ya había uno.
pub fn install_process_default() -> bool {
    let installed = build_provider().install_default().is_ok();
    if installed {
        tracing::info!(
            classical_certs = classical_certs_allowed(),
            "tls_pqc: provider PQC instalado como default del proceso (certs: ML-DSA-only \
             salvo opt-in de migración)"
        );
    } else {
        // Ya había un provider (p. ej. otro install anterior). reqwest/tonic
        // usarán ESE: si no es el PQC, los certs ML-DSA fallarán al cargar.
        tracing::warn!("tls_pqc: ya había un crypto provider default; NO se instaló el PQC");
    }
    installed
}

/// Como [`install_process_default`], pero sin tragarse el fallo: si ya había
/// otro provider, comprueba que sea equivalente al PQC (verifica ML-DSA-65 y
/// ofrece exactamente los mismos grupos de intercambio) y, si no, devuelve
/// error. Los binarios abortan con él. Seguir con un provider clásico haría
/// que cada cert ML-DSA fallase después como un `transport error` opaco — y,
/// peor, que el TLS negociase algo que no es post-cuántico sin que nadie lo
/// viera.
pub fn ensure_process_default() -> Result<(), String> {
    if install_process_default() {
        return Ok(());
    }
    let Some(current) = CryptoProvider::get_default() else {
        return Err(
            "no se pudo instalar el provider PQC y el proceso no tiene ninguno".to_string(),
        );
    };
    let ours = build_provider();
    let verifies_ml_dsa = current
        .signature_verification_algorithms
        .mapping
        .iter()
        .any(|(scheme, _)| *scheme == SignatureScheme::ML_DSA_65);
    let same_kx = current
        .kx_groups
        .iter()
        .map(|g| g.name())
        .eq(ours.kx_groups.iter().map(|g| g.name()));
    if verifies_ml_dsa && same_kx {
        Ok(())
    } else {
        Err(format!(
            "el crypto provider default del proceso no es el PQC \
             (verifica ML-DSA-65: {verifies_ml_dsa}, mismos grupos KX: {same_kx}); \
             algo instaló otro provider antes que tls_pqc"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn ensure_process_default_is_idempotent() {
        // Otro test del mismo binario puede haber instalado ya el provider:
        // en ese caso `install` falla y `ensure` tiene que reconocerlo como
        // el nuestro. Dos llamadas seguidas cubren los dos caminos.
        ensure_process_default().expect("primera llamada");
        ensure_process_default().expect("segunda llamada: ya instalado, equivalente");
    }
    use std::sync::Arc;

    #[test]
    fn provider_builds_and_maps_ml_dsa() {
        let p = pqc_crypto_provider();
        // El mapeo incluye el esquema ML_DSA_65 (lo que se ofrece/selecciona
        // en el handshake). La verificación end-to-end la cubre el test del
        // handshake; aquí basta con que el provider se construya y lo mapee.
        assert!(p
            .signature_verification_algorithms
            .mapping
            .iter()
            .any(|(scheme, _)| *scheme == SignatureScheme::ML_DSA_65));
        assert!(!p.signature_verification_algorithms.all.is_empty());
    }

    /// La política por defecto verifica SOLO ML-DSA-65; los clásicos existen
    /// únicamente detrás del opt-in de migración. Si alguien re-añade RSA a
    /// la tabla por defecto, esto lo dice antes que un despliegue.
    #[test]
    fn default_verification_is_ml_dsa_only_and_classical_is_opt_in() {
        let strict = build_provider_with(false);
        let schemes: Vec<_> = strict
            .signature_verification_algorithms
            .mapping
            .iter()
            .map(|(s, _)| *s)
            .collect();
        assert_eq!(schemes, vec![SignatureScheme::ML_DSA_65]);

        let permissive = build_provider_with(true);
        let has = |s: SignatureScheme| {
            permissive
                .signature_verification_algorithms
                .mapping
                .iter()
                .any(|(x, _)| *x == s)
        };
        assert!(has(SignatureScheme::ML_DSA_65));
        assert!(has(SignatureScheme::RSA_PSS_SHA256));
        assert!(has(SignatureScheme::ECDSA_NISTP256_SHA256));
    }

    /// End-to-end negativo: un servidor con certificado RSA (generable con
    /// CUALQUIER openssl, sin skip) no puede completar el handshake contra la
    /// política por defecto — el cliente solo anuncia ML_DSA_65 — y sí lo
    /// completa con el opt-in clásico. La carga de la clave RSA en el lado
    /// servidor pasa por el KeyProvider permisivo, que es el camino de
    /// migración real.
    #[test]
    fn rsa_certs_fail_by_default_and_work_with_the_migration_opt_in() {
        use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};

        let dir = std::env::temp_dir().join(format!("tls_rsa_optin_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = |f: &str| dir.join(f).to_str().unwrap().to_string();
        assert!(openssl(&[
            "genpkey",
            "-algorithm",
            "RSA",
            "-out",
            &p("ca.key")
        ]));
        assert!(openssl(&[
            "req",
            "-x509",
            "-key",
            &p("ca.key"),
            "-out",
            &p("ca.crt"),
            "-days",
            "2",
            "-subj",
            "/CN=rsa-ca",
        ]));
        assert!(openssl(&[
            "genpkey",
            "-algorithm",
            "RSA",
            "-out",
            &p("srv.key")
        ]));
        assert!(openssl(&[
            "req",
            "-new",
            "-key",
            &p("srv.key"),
            "-out",
            &p("srv.csr"),
            "-subj",
            "/CN=srv",
        ]));
        let ext = dir.join("srv.ext");
        std::fs::write(
            &ext,
            "subjectAltName=DNS:localhost\nextendedKeyUsage=serverAuth\n",
        )
        .unwrap();
        assert!(openssl(&[
            "x509",
            "-req",
            "-in",
            &p("srv.csr"),
            "-CA",
            &p("ca.crt"),
            "-CAkey",
            &p("ca.key"),
            "-CAcreateserial",
            "-days",
            "2",
            "-out",
            &p("srv.crt"),
            "-extfile",
            ext.to_str().unwrap(),
        ]));

        let handshake = |classical: bool| -> Result<rustls::NamedGroup, String> {
            let provider = Arc::new(build_provider_with(classical));
            let certs: Vec<CertificateDer<'static>> =
                CertificateDer::pem_file_iter(dir.join("srv.crt"))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
            let key = PrivateKeyDer::from_pem_file(dir.join("srv.key")).unwrap();
            let mut roots = rustls::RootCertStore::empty();
            for c in CertificateDer::pem_file_iter(dir.join("ca.crt")).unwrap() {
                roots.add(c.unwrap()).unwrap();
            }
            let server = rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|e| e.to_string())?;
            let client = rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .map_err(|e| e.to_string())?
                .with_root_certificates(roots)
                .with_no_client_auth();
            handshake_in_memory(Arc::new(server), Arc::new(client))
        };

        assert!(
            handshake(false).is_err(),
            "un cert RSA no debe autenticar con la política ML-DSA-only"
        );
        handshake(true).expect("con DKMS_TLS_ACCEPT_CLASSICAL_CERTS el RSA de migración funciona");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn openssl(args: &[&str]) -> bool {
        Command::new("openssl")
            .args(args)
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Genera con openssl una CA ML-DSA-65 + cert de servidor + cert de cliente
    /// (todos con firma post-cuántica) y hace un **handshake mTLS completo**
    /// in-memory con el provider PQC. Es la prueba de que la firma ML-DSA de
    /// los certificados funciona end to end. Se salta si openssl no soporta
    /// ML-DSA (necesita 3.5+).
    #[test]
    fn full_mtls_handshake_with_ml_dsa_certs_needs_openssl35() {
        use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName};

        let dir = std::env::temp_dir().join(format!("mldsa_tls_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = |f: &str| dir.join(f).to_str().unwrap().to_string();

        // Clave ML-DSA en forma **seed-only** (128 B): la que la crate `ml-dsa`
        // sabe cargar (la forma expandida/both de openssl por defecto no).
        let gen_key = |f: &str| {
            openssl(&[
                "genpkey",
                "-algorithm",
                "ML-DSA-65",
                "-provparam",
                "ml-dsa.output_formats=seed-only",
                "-out",
                &p(f),
            ])
        };

        // CA ML-DSA autofirmada.
        if !gen_key("ca.key")
            || !openssl(&[
                "req",
                "-x509",
                "-key",
                &p("ca.key"),
                "-out",
                &p("ca.crt"),
                "-days",
                "2",
                "-subj",
                "/CN=mldsa-ca",
            ])
        {
            crate::test_support::skip_or_fail(
                "openssl sin ML-DSA (<3.5): no se pueden emitir los certs del handshake",
            );
            return;
        }

        // Cert de servidor y de cliente, firmados por la CA ML-DSA.
        for (name, san, eku) in [
            ("server", "subjectAltName=DNS:localhost", "serverAuth"),
            ("client", "subjectAltName=DNS:client", "clientAuth"),
        ] {
            assert!(gen_key(&format!("{name}.key")));
            assert!(openssl(&[
                "req",
                "-new",
                "-key",
                &p(&format!("{name}.key")),
                "-out",
                &p(&format!("{name}.csr")),
                "-subj",
                &format!("/CN={name}"),
            ]));
            let ext = dir.join(format!("{name}.ext"));
            std::fs::write(&ext, format!("{san}\nextendedKeyUsage={eku}\n")).unwrap();
            assert!(openssl(&[
                "x509",
                "-req",
                "-in",
                &p(&format!("{name}.csr")),
                "-CA",
                &p("ca.crt"),
                "-CAkey",
                &p("ca.key"),
                "-CAcreateserial",
                "-days",
                "2",
                "-out",
                &p(&format!("{name}.crt")),
                "-extfile",
                ext.to_str().unwrap(),
            ]));
        }

        let load_certs = |f: &str| {
            CertificateDer::pem_file_iter(dir.join(f))
                .unwrap()
                .map(|c| c.unwrap())
                .collect::<Vec<_>>()
        };
        let load_key = |f: &str| PrivateKeyDer::from_pem_file(dir.join(f)).unwrap();

        let provider = pqc_crypto_provider();
        let mut roots = rustls::RootCertStore::empty();
        for c in load_certs("ca.crt") {
            roots.add(c).unwrap();
        }
        let roots = Arc::new(roots);

        // Servidor: exige cert cliente (mTLS), cadena de servidor ML-DSA.
        let client_verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            roots.clone(),
            provider.clone(),
        )
        .build()
        .unwrap();
        let server_config = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_client_cert_verifier(client_verifier)
            .with_single_cert(load_certs("server.crt"), load_key("server.key"))
            .unwrap();

        // Cliente: verifica al servidor con la CA ML-DSA y presenta su cert.
        let client_config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(load_certs("client.crt"), load_key("client.key"))
            .unwrap();

        // Handshake in-memory.
        let mut server = rustls::ServerConnection::new(Arc::new(server_config)).unwrap();
        let name = ServerName::try_from("localhost").unwrap();
        let mut client = rustls::ClientConnection::new(Arc::new(client_config), name).unwrap();

        // Los flights ML-DSA son grandes (cert ~4 KB + CertVerify ~3.3 KB), así
        // que `read_tls` no cabe en una sola llamada: hay que drenar el buffer
        // completo. Este macro lo hace para el lado indicado.
        macro_rules! feed {
            ($dst:expr, $data:expr, $who:expr) => {{
                let mut r: &[u8] = $data;
                while !r.is_empty() {
                    let n = $dst.read_tls(&mut r).unwrap();
                    if n == 0 {
                        break;
                    }
                    $dst.process_new_packets().expect($who);
                }
            }};
        }

        let mut buf = Vec::new();
        for _ in 0..30 {
            let mut progressed = false;
            buf.clear();
            while client.wants_write() {
                client.write_tls(&mut buf).unwrap();
                progressed = true;
            }
            if !buf.is_empty() {
                feed!(server, &buf, "server handshake");
            }
            buf.clear();
            while server.wants_write() {
                server.write_tls(&mut buf).unwrap();
                progressed = true;
            }
            if !buf.is_empty() {
                feed!(client, &buf, "client handshake");
            }
            if !client.is_handshaking() && !server.is_handshaking() {
                break;
            }
            if !progressed {
                break;
            }
        }

        assert!(!client.is_handshaking(), "cliente completó el handshake");
        assert!(!server.is_handshaking(), "servidor completó el handshake");

        // El intercambio de claves negociado tiene que ser el HÍBRIDO
        // post-cuántico, no X25519 a secas: configurarlo no basta, hay que ver
        // que se elige. Sin esto, la sesión sería vulnerable a «grabar ahora,
        // descifrar después» aunque la autenticación fuese ML-DSA.
        let kx = server
            .negotiated_key_exchange_group()
            .expect("hay grupo negociado");
        assert_eq!(
            kx.name(),
            rustls::NamedGroup::X25519MLKEM768,
            "se negoció {:?} en vez del híbrido post-cuántico",
            kx.name()
        );
        // El servidor recibió y verificó el cert cliente ML-DSA.
        assert!(
            server.peer_certificates().is_some(),
            "mTLS: cert cliente ML-DSA verificado"
        );

        // Sin respaldo clásico: un cliente que solo ofrezca X25519 no cierra
        // el handshake. Es la política de todos los planos desde 2026-08-30,
        // y lo que un SAE con OpenSSL < 3.5 va a ver.
        let server2 = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider.clone())
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(load_certs("server.crt"), load_key("server.key"))
                .unwrap(),
        );
        let mut classical = build_provider();
        classical.kx_groups = vec![aws_lc_rs::kx_group::X25519];
        let classical_client = Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(classical))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(TrustAnything))
                .with_no_client_auth(),
        );
        let err = handshake_in_memory(server2, classical_client)
            .expect_err("X25519 a secas no puede negociar");
        assert!(
            err.contains("NoKxGroupsInCommon") || err.contains("HandshakeFailure"),
            "el fallo tiene que ser por el grupo de intercambio: {err}"
        );

        // Y el self-check de arranque, con la identidad del nodo, pasa.
        self_check_hybrid_kx(
            &std::fs::read(p("server.crt")).unwrap(),
            &std::fs::read(p("server.key")).unwrap(),
        )
        .expect("self-check con la identidad del nodo");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
