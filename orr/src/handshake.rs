//! Handshake PQC por hop (wrapper finísimo sobre `common::crypto::pqc`).
//!
//! En este diseño **no hay handshake separado**: cada `encap()` ya es
//! el "saludo" — produce shared secret fresco usando la pk pública del
//! peer (que se conoce vía TOML o SDN). El receptor sólo necesita su
//! propia sk para hacer `decap()`. Mantengo estas dos funciones como
//! puntos de entrada nombrados para documentar la simetría con el
//! Python (`PQCKyberClient::encapsulate` / `PQCKyberServer::decapsulate`).

use common::crypto::pqc::{kem_for, KemEncap, PqcError};

/// Lado iniciador: encap contra la `pk` del peer. Devuelve
/// `(ciphertext, shared_secret)`. El `ciphertext` viaja al peer; el
/// `shared_secret` lo guarda el iniciador.
pub fn initiate(suite: &str, peer_public: &[u8]) -> Result<KemEncap, PqcError> {
    let kem = kem_for(suite)?;
    kem.encap(peer_public)
}

/// Lado receptor: decap del `ciphertext` con la sk local. Devuelve el
/// mismo `shared_secret` que generó el iniciador.
pub fn respond(suite: &str, my_secret: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, PqcError> {
    let kem = kem_for(suite)?;
    kem.decap(my_secret, ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::crypto::pqc::{kem_for as workspace_kem_for, suite};

    #[test]
    fn initiate_respond_match() {
        let kem = workspace_kem_for(suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        let enc = initiate(suite::ML_KEM_768, &kp.public).unwrap();
        let ss = respond(suite::ML_KEM_768, &kp.secret, &enc.ciphertext).unwrap();
        assert_eq!(ss, enc.shared_secret);
    }
}
