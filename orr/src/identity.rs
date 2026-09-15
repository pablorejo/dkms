//! Identidad PQC del ORR.
//!
//! Cada ORR genera al arrancar un par de claves ML-KEM (suite de
//! `OrrConfig::default_pqc_suite`, ML-KEM-768 por defecto). La pública se
//! sirve a los pares por `OrrControl::GetPublicKey`, firmada con la clave
//! ML-DSA-65 del certificado de nodo ([`AnnouncementSigner`]) para que el que
//! la recibe pueda encadenar hasta la CA de red en vez de confiar al primer
//! uso; la privada se queda en memoria y se zeroiza al soltar la identidad.
//! Es efímera por proceso: un reinicio produce otra, y por eso el ancla de
//! confianza es el certificado y no la clave ML-KEM. Primitivas en
//! `common::crypto::pqc`.

use common::crypto::{
    pqc::{kem_for, Kem, PqcError},
    pqc_sign::MlDsa65Signer,
};
use zeroize::Zeroizing;

/// Con qué se firma el anuncio de pubkey (`GetPublicKey`).
pub enum AnnouncementSigner {
    /// La clave ML-DSA-65 del **certificado de nodo** más su cadena DER
    /// (hoja primero). El que verifica encadena hasta la CA de red y
    /// comprueba el SAN: el ancla es el cert, que sobrevive a los reinicios
    /// de la identidad ML-KEM efímera. Es lo que hace usable
    /// `bootstrap_trust = strict` sin config por par.
    CertKey {
        key: MlDsa65Signer,
        chain: Vec<Vec<u8>>,
    },
    /// Semilla ML-DSA de config (`sign_secret_seed`), verificada con las
    /// `peer_verify_keys` que hubiera que repartir. Heredado.
    LegacySeed(Zeroizing<Vec<u8>>),
}

/// Par ML-KEM persistente del ORR mientras dure el proceso.
pub struct OrrIdentity {
    pub orr_id: String,
    pub suite: String,
    pub public_key: Vec<u8>,
    /// Clave privada ML-KEM long-term. `Zeroizing` para borrarla del heap al
    /// soltar la identidad (audit M-1): es el único secreto de larga vida que
    /// no estaba envuelto (el resto —pending_sk, ephemeral_sks, bootstrap/
    /// master secrets— ya van en Zeroizing en su almacenamiento).
    pub secret_key: Zeroizing<Vec<u8>>,
    /// Firmante del anuncio de pubkey (§Fase 6 PQC). `None` → no firma. A
    /// diferencia de la clave ML-KEM (efímera), la de firma es estable: la del
    /// cert de nodo, o la semilla heredada de config.
    pub signer: Option<AnnouncementSigner>,
    /// Firma del anuncio, calculada una vez (D-06).
    pub announcement_cache: std::sync::OnceLock<(Vec<u8>, Vec<Vec<u8>>)>,
    /// Instancia `Kem` para esta suite. Se mantiene para reutilizar el
    /// dispatch (encap/decap) sin volver a llamar a `kem_for` en cada
    /// mensaje.
    pub kem: Box<dyn Kem>,
}

impl OrrIdentity {
    /// Genera un par fresco. `suite` debe ser uno de
    /// [`common::crypto::pqc::suite`].
    pub fn generate(orr_id: impl Into<String>, suite: &str) -> Result<Self, PqcError> {
        let kem = kem_for(suite)?;
        let kp = kem.keygen()?;
        Ok(Self {
            orr_id: orr_id.into(),
            suite: kp.suite,
            public_key: kp.public,
            secret_key: Zeroizing::new(kp.secret),
            signer: None,
            announcement_cache: std::sync::OnceLock::new(),
            kem,
        })
    }

    /// Fija la semilla ML-DSA heredada (de config), salvo que ya haya un
    /// firmante atado al cert, que manda. Consuming builder.
    pub fn with_sign_seed(mut self, seed: Option<Vec<u8>>) -> Self {
        if !matches!(self.signer, Some(AnnouncementSigner::CertKey { .. })) {
            self.signer = seed.map(|s| AnnouncementSigner::LegacySeed(Zeroizing::new(s)));
        }
        self
    }

    /// Firma con la clave del certificado de nodo. Consuming builder.
    pub fn with_cert_key(mut self, key: MlDsa65Signer, chain: Vec<Vec<u8>>) -> Self {
        self.signer = Some(AnnouncementSigner::CertKey { key, chain });
        self
    }

    /// `true` si el anuncio va atado al certificado de nodo.
    pub fn signs_with_cert(&self) -> bool {
        matches!(self.signer, Some(AnnouncementSigner::CertKey { .. }))
    }

    /// Firma el anuncio `(orr_id, suite, public_key)`: `(firma, cadena de
    /// certs)` — la cadena va vacía con la semilla heredada. `None` si este
    /// ORR no firma.
    pub fn sign_pubkey_announcement(&self) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
        // `(orr_id, suite, public_key)` no cambia en la vida del proceso: se
        // firma una vez (D-06). Antes cada `GetPublicKey` costaba una firma
        // ML-DSA-65, sin reja, para cualquier cert de red.
        if let Some(c) = self.announcement_cache.get() {
            return Some(c.clone());
        }
        let signed = self.sign_pubkey_announcement_uncached()?;
        let _ = self.announcement_cache.set(signed.clone());
        Some(signed)
    }

    fn sign_pubkey_announcement_uncached(&self) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
        match self.signer.as_ref()? {
            AnnouncementSigner::CertKey { key, chain } => Some((
                common::crypto::pqc_sign::sign_orr_pubkey_with(
                    key,
                    &self.orr_id,
                    &self.suite,
                    &self.public_key,
                ),
                chain.clone(),
            )),
            AnnouncementSigner::LegacySeed(seed) => {
                match common::crypto::pqc_sign::sign_orr_pubkey(
                    seed,
                    &self.orr_id,
                    &self.suite,
                    &self.public_key,
                ) {
                    Ok(sig) => Some((sig, Vec::new())),
                    Err(e) => {
                        tracing::warn!(error = ?e, "orr: fallo al firmar el anuncio de pubkey");
                        None
                    }
                }
            }
        }
    }

    /// Tamaño en bytes del shared secret (siempre 32 para ML-KEM, pero
    /// expuesto por simetría con `public_key_len`).
    pub fn shared_secret_len(&self) -> usize {
        32
    }

    /// Decapsula con la sk de este ORR. Devuelve el shared secret
    /// (32 B).
    pub fn decap(&self, ciphertext: &[u8]) -> Result<Vec<u8>, PqcError> {
        self.kem.decap(&self.secret_key, ciphertext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::crypto::pqc::suite;

    #[test]
    fn boot_generates_keypair() {
        let id = OrrIdentity::generate("ORR_1", suite::ML_KEM_768).unwrap();
        assert!(!id.public_key.is_empty());
        assert!(!id.secret_key.is_empty());
        assert_eq!(id.suite, suite::ML_KEM_768);
    }

    #[test]
    fn round_trip_self() {
        // Verifica que un ORR puede encap contra sí mismo y luego
        // decap. No es algo que se haga en producción pero valida la
        // identidad como `Kem` round-trip.
        let id = OrrIdentity::generate("ORR_1", suite::ML_KEM_768).unwrap();
        let enc = id.kem.encap(&id.public_key).unwrap();
        let ss = id.decap(&enc.ciphertext).unwrap();
        assert_eq!(ss, enc.shared_secret);
    }
}
