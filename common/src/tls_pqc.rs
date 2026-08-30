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

/// Algoritmos de verificación: los clásicos del provider aws-lc-rs **más**
/// ML-DSA-65. `&'static` como exige `WebPkiSupportedAlgorithms`.
static VERIFY_ALGS: &[&dyn rustls::pki_types::SignatureVerificationAlgorithm] = &[
    webpki::aws_lc_rs::ML_DSA_65,
    webpki::aws_lc_rs::ECDSA_P256_SHA256,
    webpki::aws_lc_rs::ECDSA_P384_SHA384,
    webpki::aws_lc_rs::ED25519,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA256,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA384,
    webpki::aws_lc_rs::RSA_PKCS1_2048_8192_SHA512,
    webpki::aws_lc_rs::RSA_PSS_2048_8192_SHA256_LEGACY_KEY,
];

/// Mapeo `SignatureScheme → algoritmos` (TLS 1.2 y selección de esquema).
/// ML-DSA-65 más los clásicos, para no romper el mTLS RSA/ECDSA durante la
/// migración.
#[allow(clippy::type_complexity)]
static VERIFY_MAPPING: &[(
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

static PQC_KEY_PROVIDER: PqcKeyProvider = PqcKeyProvider;

/// Construye el `CryptoProvider` con ML-DSA (+ clásicos). Base:
/// `aws_lc_rs::default_provider()`.
fn build_provider() -> CryptoProvider {
    let mut provider = aws_lc_rs::default_provider();
    provider.signature_verification_algorithms = rustls::crypto::WebPkiSupportedAlgorithms {
        all: VERIFY_ALGS,
        mapping: VERIFY_MAPPING,
    };
    provider.key_provider = &PQC_KEY_PROVIDER;

    // Intercambio de claves HÍBRIDO post-cuántico primero.
    //
    // Autenticar con ML-DSA protege de que alguien se haga pasar por un nodo,
    // pero no de «grabar ahora, descifrar después»: si el secreto de sesión se
    // acuerda solo con X25519, un adversario con ordenador cuántico puede
    // guardar el tráfico de hoy y descifrarlo mañana. Y por este TLS viajan
    // claves de sesión de SAE.
    //
    // rustls trae `X25519MLKEM768` (draft-ietf-tls-ecdhe-mlkem) pero solo lo
    // ofrece por defecto con la feature `prefer-post-quantum`, que no está
    // activa aquí — así que lo ponemos delante explícitamente. Es híbrido: el
    // secreto sale de combinar X25519 **y** ML-KEM-768, de modo que sigue
    // siendo tan seguro como X25519 aunque ML-KEM fallara, y resistente a
    // cuántico aunque X25519 caiga. Los clásicos quedan detrás para no romper
    // a un peer que aún no lo soporte.
    provider.kx_groups = vec![
        aws_lc_rs::kx_group::X25519MLKEM768,
        aws_lc_rs::kx_group::X25519,
        aws_lc_rs::kx_group::SECP256R1,
        aws_lc_rs::kx_group::SECP384R1,
    ];
    provider
}

/// Provider criptográfico con firma ML-DSA en certificados, además de los
/// algoritmos clásicos.
pub fn pqc_crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(build_provider())
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
            "tls_pqc: provider PQC (ML-DSA + clásicos) instalado como default del proceso"
        );
    } else {
        // Ya había un provider (p. ej. otro install anterior). reqwest/tonic
        // usarán ESE: si no es el PQC, los certs ML-DSA fallarán al cargar.
        tracing::warn!("tls_pqc: ya había un crypto provider default; NO se instaló el PQC");
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
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
    fn full_mtls_handshake_with_ml_dsa_certs() {
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
            eprintln!("openssl sin ML-DSA (¿<3.5?); salto el test de handshake");
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
        let _ = std::fs::remove_dir_all(&dir);
    }
}
