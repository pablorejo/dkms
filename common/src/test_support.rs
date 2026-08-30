//! Ayudas para tests que dependen del entorno.
//!
//! Un test que no puede correr donde está (el `openssl` del sistema no sabe
//! ML-DSA, el backend de LP no es el que el test exige) tiene que decirlo en
//! voz alta, no terminar en verde sin haber comprobado nada. Vive fuera de
//! `#[cfg(test)]` porque los tests de otros crates también lo usan.

/// Registra un salto o, con `DKMS_NO_TEST_SKIPS=1` (lo pone la CI), lo
/// convierte en fallo: así un verde significa que todo corrió de verdad.
///
/// El llamador hace `return` justo después; esta función no lo hace por él
/// para que el salto quede visible en el propio test.
pub fn skip_or_fail(reason: &str) {
    if std::env::var_os("DKMS_NO_TEST_SKIPS").is_some() {
        panic!("test saltado con DKMS_NO_TEST_SKIPS activo: {reason}");
    }
    eprintln!("SKIPPED: {reason}");
}

/// PKI ML-DSA-65 de prueba emitida con el `openssl` del sistema: una CA de
/// red y un cert por nodo con `SAN URI:dkms://<id>` y EKU cliente+servidor,
/// como los de `docker/gen-certs.sh`. `None` si ese openssl no sabe ML-DSA
/// (< 3.5) — el llamador decide si salta o falla con [`skip_or_fail`].
pub struct TestPki {
    pub dir: std::path::PathBuf,
    pub ca_crt: std::path::PathBuf,
}

impl TestPki {
    pub fn cert(&self, id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{id}.crt"))
    }
    pub fn key(&self, id: &str) -> std::path::PathBuf {
        self.dir.join(format!("{id}.key"))
    }
    /// La hoja en DER (primer bloque PEM del `.crt`).
    pub fn cert_der(&self, id: &str) -> Vec<u8> {
        use rustls::pki_types::{pem::PemObject, CertificateDer};
        let pem = std::fs::read(self.cert(id)).expect("cert de prueba");
        CertificateDer::from_pem_slice(&pem)
            .expect("PEM")
            .as_ref()
            .to_vec()
    }
}

pub fn mldsa_test_pki(dir: &std::path::Path, node_ids: &[&str]) -> Option<TestPki> {
    use std::process::Command;
    let _ = std::fs::create_dir_all(dir);
    let p = |f: &str| dir.join(f).to_str().unwrap().to_string();
    let openssl = |args: &[&str]| {
        Command::new("openssl")
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
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
    if !gen_key("net-ca.key")
        || !openssl(&[
            "req",
            "-x509",
            "-key",
            &p("net-ca.key"),
            "-out",
            &p("net-ca.crt"),
            "-days",
            "2",
            "-subj",
            "/CN=net-ca",
            "-addext",
            "basicConstraints=critical,CA:TRUE",
        ])
    {
        return None;
    }
    for id in node_ids {
        assert!(gen_key(&format!("{id}.key")));
        assert!(openssl(&[
            "req",
            "-new",
            "-key",
            &p(&format!("{id}.key")),
            "-out",
            &p(&format!("{id}.csr")),
            "-subj",
            &format!("/CN={id}"),
        ]));
        let ext = dir.join(format!("{id}.ext"));
        std::fs::write(
            &ext,
            format!(
                "subjectAltName=URI:dkms://{id},DNS:localhost\n\
                 extendedKeyUsage=serverAuth,clientAuth\n"
            ),
        )
        .unwrap();
        assert!(openssl(&[
            "x509",
            "-req",
            "-in",
            &p(&format!("{id}.csr")),
            "-CA",
            &p("net-ca.crt"),
            "-CAkey",
            &p("net-ca.key"),
            "-CAcreateserial",
            "-out",
            &p(&format!("{id}.crt")),
            "-days",
            "2",
            "-extfile",
            ext.to_str().unwrap(),
        ]));
    }
    Some(TestPki {
        dir: dir.to_path_buf(),
        ca_crt: dir.join("net-ca.crt"),
    })
}
