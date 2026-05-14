//! Directorio de peers ORR.
//!
//! Equivalente al `PeerDirectory` del ORR Python: dado un `orr_id` lógico,
//! devuelve dos cosas:
//!
//!   * `qkc_id`: el QKC al que está pegado ese ORR — lo que va en
//!     `dest_final` de los frames hacia el QKC.
//!   * `public_key`: la clave pública ML-KEM del ORR — necesaria para
//!     hacer `encap` cuando armamos una capa onion contra ese peer.
//!
//! Sembrado al arrancar desde TOML (`orr/config/default.toml`) — el
//! Python lo hacía leyendo ficheros JSON por peer.
//! Mutables en caliente: la SDN (otra sesión) podrá empujar updates
//! por gRPC en el futuro.

use std::collections::HashMap;

use parking_lot::RwLock;

pub struct PeerRegistry {
    by_orr:       RwLock<HashMap<String, u32>>,     // orr_id → qkc_id
    pubkeys:      RwLock<HashMap<String, Vec<u8>>>, // orr_id → ML-KEM pubkey
    local_orr_id: String,
    local_qkc_id: u32,
}

impl PeerRegistry {
    pub fn new(seed_qkc: HashMap<String, u32>, local_orr_id: String, local_qkc_id: u32) -> Self {
        Self {
            by_orr:  RwLock::new(seed_qkc),
            pubkeys: RwLock::new(HashMap::new()),
            local_orr_id,
            local_qkc_id,
        }
    }

    /// Constructor con ambos sembrados a la vez. Lo usa `OrrService::new`
    /// para inyectar los pubkeys decodificados desde TOML.
    pub fn with_pubkeys(
        seed_qkc: HashMap<String, u32>,
        seed_pubkeys: HashMap<String, Vec<u8>>,
        local_orr_id: String,
        local_qkc_id: u32,
    ) -> Self {
        Self {
            by_orr:  RwLock::new(seed_qkc),
            pubkeys: RwLock::new(seed_pubkeys),
            local_orr_id,
            local_qkc_id,
        }
    }

    /// `qkc_id` del ORR indicado. Si el peer somos nosotros, devuelve el
    /// propio `qkc_id`. Si el peer no está registrado devuelve `None` —
    /// el caller decide si fallar o caer a un fallback (p.ej. consultar
    /// a la SDN).
    pub fn qkc_id(&self, orr_id: &str) -> Option<u32> {
        if orr_id == self.local_orr_id {
            return Some(self.local_qkc_id);
        }
        self.by_orr.read().get(orr_id).copied()
    }

    pub fn put(&self, orr_id: String, qkc_id: u32) {
        self.by_orr.write().insert(orr_id, qkc_id);
    }

    pub fn remove(&self, orr_id: &str) {
        self.by_orr.write().remove(orr_id);
    }

    /// Clave pública ML-KEM del peer. `None` si no la conocemos
    /// (caller decide: pedirla por SDN, fallar, etc.).
    pub fn public_key(&self, orr_id: &str) -> Option<Vec<u8>> {
        self.pubkeys.read().get(orr_id).cloned()
    }

    pub fn put_pubkey(&self, orr_id: String, pubkey: Vec<u8>) {
        self.pubkeys.write().insert(orr_id, pubkey);
    }

    pub fn remove_pubkey(&self, orr_id: &str) {
        self.pubkeys.write().remove(orr_id);
    }

    pub fn local_orr_id(&self) -> &str {
        &self.local_orr_id
    }

    pub fn local_qkc_id(&self) -> u32 {
        self.local_qkc_id
    }

    /// Vista inmutable de todos los peers conocidos. Útil para
    /// /healthz, debugging y métricas.
    pub fn snapshot(&self) -> HashMap<String, u32> {
        self.by_orr.read().clone()
    }

    /// Vista inmutable de los pubkeys conocidos.
    pub fn snapshot_pubkeys(&self) -> HashMap<String, Vec<u8>> {
        self.pubkeys.read().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_self_lookup() {
        let reg = PeerRegistry::new(HashMap::new(), "ORR_1".into(), 1);
        assert_eq!(reg.qkc_id("ORR_1"), Some(1));
        assert_eq!(reg.qkc_id("ORR_2"), None);
    }

    #[test]
    fn put_and_get() {
        let reg = PeerRegistry::new(HashMap::new(), "ORR_1".into(), 1);
        reg.put("ORR_3".into(), 7);
        assert_eq!(reg.qkc_id("ORR_3"), Some(7));
    }

    #[test]
    fn pubkey_storage() {
        let reg = PeerRegistry::new(HashMap::new(), "ORR_1".into(), 1);
        assert_eq!(reg.public_key("ORR_2"), None);
        reg.put_pubkey("ORR_2".into(), vec![0xAA; 32]);
        assert_eq!(reg.public_key("ORR_2"), Some(vec![0xAA; 32]));
        reg.remove_pubkey("ORR_2");
        assert_eq!(reg.public_key("ORR_2"), None);
    }

    #[test]
    fn with_pubkeys_seeds_both() {
        let mut q = HashMap::new();
        q.insert("ORR_2".into(), 2);
        let mut p = HashMap::new();
        p.insert("ORR_2".into(), vec![0xBB; 16]);
        let reg = PeerRegistry::with_pubkeys(q, p, "ORR_1".into(), 1);
        assert_eq!(reg.qkc_id("ORR_2"), Some(2));
        assert_eq!(reg.public_key("ORR_2"), Some(vec![0xBB; 16]));
    }
}
