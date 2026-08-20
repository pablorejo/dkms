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
        match self.take_diagnosed(peer, key_id) {
            TakeOutcome::Hit(e) => Some(e),
            _ => None,
        }
    }

    /// Como [`take`](Self::take) pero distingue POR QUÉ falló. Los dos
    /// fallos piden arreglos opuestos y antes eran indistinguibles: un
    /// `UnknownPeer` es un desajuste de identidad (el `from` del ACK no es
    /// la clave con la que tenemos al peer), un `UnknownKey` es un ACK
    /// tardío que el reaper ya expiró.
    pub fn take_diagnosed(&self, peer: &str, key_id: &KeyId) -> TakeOutcome {
        let mut guard = self.inner.lock();
        let Some(bucket) = guard.get_mut(peer) else {
            return TakeOutcome::UnknownPeer;
        };
        let entry = bucket.remove(key_id);
        if bucket.is_empty() {
            guard.remove(peer);
        }
        match entry {
            Some(e) => TakeOutcome::Hit(e),
            None => TakeOutcome::UnknownKey,
        }
    }

    /// Peers con al menos una entrada pendiente. Sólo para diagnóstico:
    /// al recibir un ACK con un `from` desconocido, se loguea junto a esta
    /// lista para que el desajuste de identidad salte a la vista.
    pub fn peers(&self) -> Vec<String> {
        self.inner.lock().keys().cloned().collect()
    }

    /// Devuelve cuántas entradas pendientes hay para un peer concreto.
    /// Tira todo lo pendiente de ACK de `peer` y devuelve cuánto era.
    ///
    /// Para cuando el peer se reinicia: esas claves ya viajaron, y quien
    /// tenía que acusarlas recibo ya no existe. Esperar a que expiren cuenta
    /// contra el tope de emisión (`enc + pending >= capacity`), así que sin
    /// esto el generador seguiría parado un TTL entero después de saber que
    /// hay que rellenar.
    pub fn drop_peer(&self, peer: &str) -> usize {
        self.inner
            .lock()
            .remove(peer)
            .map(|entries| entries.len())
            .unwrap_or(0)
    }

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

    /// Recorre y elimina las entradas expiradas. Devuelve **cuántas por
    /// peer**: un total agregado no dice si se está perdiendo todo hacia
    /// un peer concreto o un poco hacia todos, que es justo lo que hay que
    /// saber. Sólo aparecen los peers con al menos una expiración.
    pub fn reap_expired_by_peer(&self, now: Instant) -> Vec<(String, usize)> {
        let mut removed: Vec<(String, usize)> = Vec::new();
        let mut guard = self.inner.lock();
        guard.retain(|peer, bucket| {
            let before = bucket.len();
            bucket.retain(|_kid, entry| !entry.is_expired(now));
            let n = before - bucket.len();
            if n > 0 {
                removed.push((peer.clone(), n));
            }
            !bucket.is_empty()
        });
        removed
    }

    /// Total de entradas expiradas. Envoltorio de
    /// [`reap_expired_by_peer`](Self::reap_expired_by_peer).
    pub fn reap_expired(&self, now: Instant) -> usize {
        self.reap_expired_by_peer(now).iter().map(|(_, n)| n).sum()
    }
}

/// Resultado de [`AckPendingStore::take_diagnosed`].
pub enum TakeOutcome {
    /// El ACK casó: aquí está la clave, lista para `buffer_enc`.
    Hit(AckPendingEntry),
    /// No hay ninguna entrada pendiente para ese peer.
    UnknownPeer,
    /// El peer existe pero ese `key_id` ya no: expirado o ACK duplicado.
    UnknownKey,
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
        let entry =
            AckPendingEntry::new(vec![0xAB; 32], now + Duration::from_secs(30), KeyGrade::Qkd);
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
    fn take_diagnosed_separates_unknown_peer_from_unknown_key() {
        let store = AckPendingStore::new();
        let now = Instant::now();
        store.insert(
            "dkms-22",
            id("k1"),
            AckPendingEntry::new(vec![0xAB; 32], now + Duration::from_secs(30), KeyGrade::Qkd),
        );
        // ACK con un `from` que no conocemos → desajuste de identidad.
        assert!(matches!(
            store.take_diagnosed("DKMS-22", &id("k1")),
            TakeOutcome::UnknownPeer
        ));
        // Peer correcto, key_id que ya no está → ACK tardío / duplicado.
        assert!(matches!(
            store.take_diagnosed("dkms-22", &id("otra")),
            TakeOutcome::UnknownKey
        ));
        assert!(matches!(
            store.take_diagnosed("dkms-22", &id("k1")),
            TakeOutcome::Hit(_)
        ));
    }

    #[test]
    fn reap_expired_reports_per_peer() {
        let store = AckPendingStore::new();
        let now = Instant::now();
        let dead =
            || AckPendingEntry::new(vec![0xEF; 32], now - Duration::from_secs(1), KeyGrade::Qkd);
        store.insert("dkms-2", id("a"), dead());
        store.insert("dkms-2", id("b"), dead());
        store.insert("dkms-3", id("c"), dead());
        let mut by_peer = store.reap_expired_by_peer(now);
        by_peer.sort();
        assert_eq!(
            by_peer,
            vec![("dkms-2".to_string(), 2), ("dkms-3".to_string(), 1)]
        );
    }

    #[test]
    fn reap_expired_drops_old_entries() {
        let store = AckPendingStore::new();
        let now = Instant::now();
        let alive =
            AckPendingEntry::new(vec![0xCD; 32], now + Duration::from_secs(30), KeyGrade::Qkd);
        let dead =
            AckPendingEntry::new(vec![0xEF; 32], now - Duration::from_secs(1), KeyGrade::Qkd);
        store.insert("dkms-22", id("alive"), alive);
        store.insert("dkms-22", id("dead"), dead);
        let n = store.reap_expired(now);
        assert_eq!(n, 1);
        assert!(store.take("dkms-22", &id("dead")).is_none());
        assert!(store.take("dkms-22", &id("alive")).is_some());
    }

    /// Un peer que se reinicia deja pendientes ACK que ya nadie va a mandar.
    /// Cuentan contra el tope de emisión, así que hay que soltarlos al
    /// enterarse y no esperar a que expiren.
    #[test]
    fn dropping_a_peer_releases_everything_pending_for_it() {
        let store = AckPendingStore::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        for k in ["k1", "k2", "k3"] {
            store.insert(
                "dkms-2",
                KeyId::new(k),
                AckPendingEntry::new(vec![0u8; 32], deadline, KeyGrade::Pqc),
            );
        }
        store.insert(
            "dkms-3",
            KeyId::new("otra"),
            AckPendingEntry::new(vec![0u8; 32], deadline, KeyGrade::Pqc),
        );

        assert_eq!(store.drop_peer("dkms-2"), 3);
        assert_eq!(store.pending_count("dkms-2"), 0);
        assert_eq!(
            store.pending_count("dkms-3"),
            1,
            "los demás peers no se tocan",
        );
        assert_eq!(store.drop_peer("dkms-2"), 0, "idempotente");
    }
}
