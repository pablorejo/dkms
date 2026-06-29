//! Tabla `ack_pending` por peer: claves emitidas que esperan ACK.
//!
//! Cuando el Generator emite una clave hacia el peer B, inserta una
//! `AckPendingEntry` en `ack_pending[B][key_id]`. Esta entrada vive hasta
//! una de tres cosas:
//!
//!   1. **ACK recibido por TCP** → la entrada se mueve a
//!      `BufferPool.enc[B]` y la clave ya es elegible para el flujo ETSI.
//!   2. **Deadline excedido** (`reaper`) → la entrada se descarta y la
//!      clave se zeroiza; típicamente significa que el ORR/QKC perdió el
//!      frame o el ACK socket no respondió.
//!   3. **Shutdown** → todas las entradas se purgan al cerrar el DKMS.
//!
//! Concurrencia: el store usa `parking_lot::Mutex` sobre un `HashMap`
//! anidado. Las operaciones son O(1) amortizado (HashMap).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use zeroize::Zeroizing;

use common::ids::KeyId;
use common::security::KeyGrade;

/// Una clave a la espera de ACK desde el peer.
pub struct AckPendingEntry {
    /// Bytes brutos de la clave. `Zeroizing` los borra al drop.
    pub bytes: Zeroizing<Vec<u8>>,
    /// Instante monotónico tras el cual la entrada expira.
    pub deadline: Instant,
    /// Grado con el que se bombeó la clave: al recibir el ACK, `on_ack` la
    /// mueve al buffer ENC de ESTE grado (`enc_qkd` vs `enc_pqc`).
    pub grade: KeyGrade,
}

impl AckPendingEntry {
    pub fn new(bytes: Vec<u8>, deadline: Instant, grade: KeyGrade) -> Self {
        Self {
            bytes: Zeroizing::new(bytes),
            deadline,
            grade,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        now >= self.deadline
    }
}

/// Tabla `ack_pending` global del DKMS. Indexada por (peer_dkms_id, key_id).
#[derive(Default)]
pub struct AckPendingStore {
    /// `peer_dkms_id → key_id → entry`. Nested HashMap para que el reaper
    /// pueda barrer un peer concreto sin tocar los demás.
    inner: Mutex<HashMap<String, HashMap<KeyId, AckPendingEntry>>>,
}

impl AckPendingStore {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Inserta una entrada. Si ya existía una clave con el mismo `key_id`
    /// para ese peer (no debería, los UUIDs son únicos), se sobreescribe.
    pub fn insert(&self, peer: &str, key_id: KeyId, entry: AckPendingEntry) {
        let mut guard = self.inner.lock();
        guard
            .entry(peer.to_owned())
            .or_default()
            .insert(key_id, entry);
    }

    /// Saca la entrada y devuelve sus bytes si existía. Llamada por la
    /// vía ACK al recibir confirmación del peer.
    pub fn take(&self, peer: &str, key_id: &KeyId) -> Option<AckPendingEntry> {
        let mut guard = self.inner.lock();
        let bucket = guard.get_mut(peer)?;
        let entry = bucket.remove(key_id);
        if bucket.is_empty() {
            guard.remove(peer);
        }
        entry
    }

    /// Devuelve cuántas entradas pendientes hay para un peer concreto.
    pub fn pending_count(&self, peer: &str) -> usize {
        self.inner.lock().get(peer).map(|m| m.len()).unwrap_or(0)
    }

    /// Snapshot agregado: lista de (peer, count). Útil para métricas y
    /// `/healthz`.
    pub fn snapshot(&self) -> Vec<(String, usize)> {
        self.inner
            .lock()
            .iter()
            .map(|(p, m)| (p.clone(), m.len()))
            .collect()
    }

    /// Recorre y elimina las entradas expiradas. Devuelve el número de
    /// entradas eliminadas para que el caller pueda incrementar métricas.
    pub fn reap_expired(&self, now: Instant) -> usize {
        let mut removed = 0;
        let mut guard = self.inner.lock();
        guard.retain(|_peer, bucket| {
            bucket.retain(|_kid, entry| {
                let alive = !entry.is_expired(now);
                if !alive {
                    removed += 1;
                }
                alive
            });
            !bucket.is_empty()
        });
        removed
    }
}

/// Arc<AckPendingStore> es el tipo que comparten Generator + ack socket.
pub type SharedAckPending = Arc<AckPendingStore>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn id(s: &str) -> KeyId {
        KeyId::new(s)
    }

    #[test]
    fn insert_and_take() {
        let store = AckPendingStore::new();
        let now = Instant::now();
        let entry = AckPendingEntry::new(vec![0xAB; 32], now + Duration::from_secs(30), KeyGrade::Qkd);
        store.insert("dkms-22", id("k1"), entry);
        assert_eq!(store.pending_count("dkms-22"), 1);
        let got = store.take("dkms-22", &id("k1")).expect("present");
        assert_eq!(got.bytes.as_slice(), &[0xAB; 32]);
        assert_eq!(store.pending_count("dkms-22"), 0);
    }

    #[test]
    fn take_missing_returns_none() {
        let store = AckPendingStore::new();
        assert!(store.take("dkms-22", &id("nope")).is_none());
    }

    #[test]
    fn reap_expired_drops_old_entries() {
        let store = AckPendingStore::new();
        let now = Instant::now();
        let alive = AckPendingEntry::new(vec![0xCD; 32], now + Duration::from_secs(30), KeyGrade::Qkd);
        let dead = AckPendingEntry::new(vec![0xEF; 32], now - Duration::from_secs(1), KeyGrade::Qkd);
        store.insert("dkms-22", id("alive"), alive);
        store.insert("dkms-22", id("dead"), dead);
        let n = store.reap_expired(now);
        assert_eq!(n, 1);
        assert!(store.take("dkms-22", &id("dead")).is_none());
        assert!(store.take("dkms-22", &id("alive")).is_some());
    }
}
