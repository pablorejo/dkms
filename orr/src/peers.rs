//! Directorio de peers ORR.
//!
//! Equivalente al `PeerDirectory` del ORR Python: dado un `orr_id` lógico,
//! devuelve:
//!
//!   * `qkc_id`: el QKC al que está pegado ese ORR — lo que va en
//!     `dest_final` de los frames hacia el QKC.
//!   * `public_key`: la clave pública ML-KEM del ORR — necesaria para
//!     hacer `encap` UNA VEZ al arrancar contra ese peer y generar el
//!     `master_secret`.
//!   * `master_secret`: shared secret de 32 B compartido con ese peer.
//!     Establecido por el bootstrap (`bootstrap.rs`) y consumido por
//!     `onion::derive_key` para producir K per-frame.
//!
//! Sembrado al arrancar desde TOML (`orr/config/default.toml`); pubkeys y
//! master_secrets se rellenan por gRPC (`GetPublicKey` + `EstablishSecret`).
//! Mutables en caliente: la SDN (otra sesión) podrá empujar updates
//! por gRPC en el futuro.

use std::collections::HashMap;

use parking_lot::RwLock;

pub struct PeerRegistry {
    by_orr:         RwLock<HashMap<String, u32>>,        // orr_id → qkc_id
    pubkeys:        RwLock<HashMap<String, Vec<u8>>>,    // orr_id → ML-KEM pubkey
    /// orr_id → master_secret 32 B compartido con ese peer (vía ML-KEM
    /// encap al arrancar). `derive_key` hace HKDF-SHA256 sobre estos
    /// 32 B + key_id + body_len para producir la K per-frame.
    master_secrets: RwLock<HashMap<String, [u8; 32]>>,
    local_orr_id:   String,
    local_qkc_id:   u32,
}

impl PeerRegistry {
    pub fn new(seed_qkc: HashMap<String, u32>, local_orr_id: String, local_qkc_id: u32) -> Self {
        Self {
            by_orr:         RwLock::new(seed_qkc),
            pubkeys:        RwLock::new(HashMap::new()),
            master_secrets: RwLock::new(HashMap::new()),
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
            by_orr:         RwLock::new(seed_qkc),
            pubkeys:        RwLock::new(seed_pubkeys),
            master_secrets: RwLock::new(HashMap::new()),
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

    /// Master_secret 32 B compartido con el peer. `None` si todavía no
    /// hicimos el handshake (caller debe esperar o fallar).
    pub fn master_secret(&self, orr_id: &str) -> Option<[u8; 32]> {
        self.master_secrets.read().get(orr_id).copied()
    }

    pub fn put_master_secret(&self, orr_id: String, secret: [u8; 32]) {
        self.master_secrets.write().insert(orr_id, secret);
    }

    pub fn has_master_secret(&self, orr_id: &str) -> bool {
        self.master_secrets.read().contains_key(orr_id)
    }

    /// Snapshot del set de peers con master_secret establecido. Útil
    /// para `/healthz` y diagnóstico de bootstrap incompleto.
    pub fn peers_with_secret(&self) -> Vec<String> {
        let mut v: Vec<String> = self.master_secrets.read().keys().cloned().collect();
        v.sort_unstable();
        v
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

    #[test]
    fn master_secret_storage() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert!(reg.master_secret("orr_2").is_none());
        assert!(!reg.has_master_secret("orr_2"));
        let s = [0xCC; 32];
        reg.put_master_secret("orr_2".into(), s);
        assert_eq!(reg.master_secret("orr_2"), Some(s));
        assert!(reg.has_master_secret("orr_2"));
        assert_eq!(reg.peers_with_secret(), vec!["orr_2"]);
    }
}
