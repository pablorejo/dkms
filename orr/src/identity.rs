//! Identidad PQC del ORR.
//!
//! Cada ORR genera al arrancar un par de claves ML-KEM (suite del
//! `OrrConfig::default_pqc_suite`, ML-KEM-768 por defecto). La clave
//! pública se publica vía `OrrControl::GetPublicKey` (cuando se cablee)
//! o se sirve a peers a través de la SDN; la privada se queda en
//! memoria.
//!
//! Análogo del Python: `PQCServer` cuyo `__init__` hace
//! `kyber.generate_keypair()` y guarda `(public_key, private_key)`. Lo
//! que en el Python era `Kyber("Kyber1024")` aquí es
//! `pqc::kem_for("ml-kem-1024")`. Tira de las primitivas de
//! `common::crypto::pqc`.

use common::crypto::pqc::{kem_for, Kem, PqcError};
use zeroize::Zeroizing;

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
    /// Semilla ML-DSA (32 B) de la identidad de **firma** estable de este ORR
    /// (§Fase 6 PQC). `None` → no firma sus anuncios de pubkey. A diferencia de
    /// la clave ML-KEM (efímera), esta viene de config y es estable, así que el
    /// peer puede verificarla con una clave pública fija.
    pub sign_seed: Option<Zeroizing<Vec<u8>>>,
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
            sign_seed: None,
            kem,
        })
    }

    /// Fija la semilla ML-DSA de firma (de config). Consuming builder.
    pub fn with_sign_seed(mut self, seed: Option<Vec<u8>>) -> Self {
        self.sign_seed = seed.map(Zeroizing::new);
        self
    }

    /// Firma el anuncio `(orr_id, suite, public_key)` con la identidad ML-DSA,
    /// si está configurada. `None` si este ORR no tiene semilla de firma.
    pub fn sign_pubkey_announcement(&self) -> Option<Vec<u8>> {
        let seed = self.sign_seed.as_deref()?;
        match common::crypto::pqc_sign::sign_orr_pubkey(
            seed,
            &self.orr_id,
            &self.suite,
            &self.public_key,
        ) {
            Ok(sig) => Some(sig),
            Err(e) => {
                tracing::warn!(error = ?e, "orr: fallo al firmar el anuncio de pubkey");
                None
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
