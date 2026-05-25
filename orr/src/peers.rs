//! Directorio de peers ORR.
//!
//! Equivalente al `PeerDirectory` del ORR Python: dado un `orr_id` lógico,
//! devuelve:
//!
//!   * `qkc_id`: el QKC al que está pegado ese ORR — lo que va en
//!     `dest_final` de los frames hacia el QKC.
//!   * `public_key`: la clave pública ML-KEM long-term del ORR —
//!     necesaria para hacer `encap` UNA vez en el bootstrap inicial
//!     (audit H-3: nunca más después).
//!   * `bootstrap_secret`: shared secret de 32 B compartido con ese peer
//!     tras el bootstrap inicial. **Sólo se usa como clave HMAC** para
//!     autenticar las RPCs de rotación. NUNCA como keystream.
//!   * `master_secrets[epoch_id]`: shared secret de 32 B por época,
//!     resultado de cada rotación (`RequestEphemeralKey` +
//!     `EstablishEphemeralSecret`). Indexado por época monotónica.
//!     Consumido por `onion::derive_key` como `ikm` del HKDF.
//!   * `ephemeral_sks[epoch_id]` (solo lado responder): la `esk` ML-KEM
//!     efímera generada al recibir `RequestEphemeralKey`. Se ZEROIZA
//!     inmediatamente tras decapsular el `EstablishEphemeralSecret` →
//!     forward secrecy boundary (audit H-3 / Option B).
//!   * `current_send_epoch[peer]` (solo lado initiator): la última época
//!     que negoció con el peer. La siguiente rotación usa
//!     `current_send_epoch + 1`.
//!
//! ## Threat model y forward secrecy
//!
//! Sin rotación, si un atacante captura la long-term sk del ORR en el
//! futuro y tiene tráfico grabado pasado, descifra TODO el histórico.
//! Con Option B:
//!   1. Bootstrap inicial deriva un `bootstrap_secret` que se usa SOLO
//!      como HMAC key (autenticación, no cifrado).
//!   2. Cada rotación genera un `master_secret` nuevo con una keypair
//!      ML-KEM efímera. La esk del responder se zeroiza tras decap.
//!   3. Capturar la long-term sk en el futuro no permite recomputar
//!      ningún `master_secret` pasado (esas esks ya no existen).
//!
//! El estado se mantiene en memoria. Todo material secreto va envuelto
//! en `Zeroizing<...>` para borrarse al drop (regla CLAUDE.md: RAM-only
//! + zeroize). Tras reinicio del proceso, se rehace el bootstrap inicial.

use std::collections::{BTreeMap, HashMap};
use std::time::Instant;

use parking_lot::RwLock;
use zeroize::Zeroizing;

pub struct PeerRegistry {
    by_orr: RwLock<HashMap<String, u32>>,      // orr_id → qkc_id
    pubkeys: RwLock<HashMap<String, Vec<u8>>>, // orr_id → ML-KEM long-term pubkey
    /// orr_id → `bootstrap_secret` 32 B compartido con ese peer (vía
    /// ML-KEM encap contra la long-term pk en el bootstrap inicial).
    /// USO ÚNICO: clave HMAC para autenticar `RequestEphemeralKey` /
    /// `EstablishEphemeralSecret` (etiquetas de dominio `"REQ"`,
    /// `"RESP"`, `"FIN"`). Nunca como keystream.
    bootstrap_secrets: RwLock<HashMap<String, Zeroizing<[u8; 32]>>>,
    /// orr_id → mapa de épocas. Cada época tiene su `master_secret`
    /// 32 B propio (producto de una rotación con keypair ML-KEM
    /// efímera fresca). El BTreeMap permite iterar en orden de épocas
    /// y dropear las viejas con `split_off`. El XOR del onion usa
    /// `master_secrets[from][frame.epoch_id]`.
    master_secrets: RwLock<HashMap<String, BTreeMap<u32, Zeroizing<[u8; 32]>>>>,
    /// orr_id → `ephemeral_sks[epoch_id]` (solo en el lado responder
    /// de cada rotación). Vive desde que el responder genera la
    /// keypair efímera (handler de `RequestEphemeralKey`) hasta que
    /// recibe `EstablishEphemeralSecret` y decapsula. Se ZEROIZA
    /// inmediatamente tras decap exitoso → forward secrecy boundary.
    ephemeral_sks: RwLock<HashMap<String, BTreeMap<u32, Zeroizing<Vec<u8>>>>>,
    /// orr_id → última `epoch_id` que **este** ORR negoció como
    /// initiator con el peer. Solo se mantiene en el lado lex-smaller
    /// (= initiator). La siguiente rotación usa este valor + 1.
    current_send_epochs: RwLock<HashMap<String, u32>>,
    /// orr_id → último `Instant` en que un re-bootstrap reactivo
    /// **FALLÓ** contra ese peer. Sirve para rate-limit (ver
    /// `should_attempt_rebootstrap`) — el rate-limit solo aplica si
    /// el intento anterior fracasó, así un primer intento (o un retry
    /// tras éxito previo) procede sin espera.
    ///
    /// Cambio 2026-05-25 (optimización fix bootstrap): antes era
    /// `last_attempt` y se marcaba SIEMPRE, lo que añadía 30 s de
    /// latencia incluso cuando el intento previo había sido exitoso —
    /// si el pod del peer reiniciaba poco después, había que esperar
    /// el rate-limit aunque no hubiera ningún problema en curso.
    rebootstrap_last_failure: RwLock<HashMap<String, Instant>>,
    /// orr_id → flag "rebootstrap en vuelo". Sirve para deduplicar: si
    /// llegan 100 frames undecryptable de orr_X en 50 ms, solo el
    /// primero dispara el handshake; los demás ven el flag y se
    /// limitan a dropear.
    rebootstrap_inflight: RwLock<HashMap<String, bool>>,
    local_orr_id: String,
    local_qkc_id: u32,
}

impl PeerRegistry {
    pub fn new(seed_qkc: HashMap<String, u32>, local_orr_id: String, local_qkc_id: u32) -> Self {
        Self {
            by_orr: RwLock::new(seed_qkc),
            pubkeys: RwLock::new(HashMap::new()),
            bootstrap_secrets: RwLock::new(HashMap::new()),
            master_secrets: RwLock::new(HashMap::new()),
            ephemeral_sks: RwLock::new(HashMap::new()),
            current_send_epochs: RwLock::new(HashMap::new()),
            rebootstrap_last_failure: RwLock::new(HashMap::new()),
            rebootstrap_inflight: RwLock::new(HashMap::new()),
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
            by_orr: RwLock::new(seed_qkc),
            pubkeys: RwLock::new(seed_pubkeys),
            bootstrap_secrets: RwLock::new(HashMap::new()),
            master_secrets: RwLock::new(HashMap::new()),
            ephemeral_sks: RwLock::new(HashMap::new()),
            current_send_epochs: RwLock::new(HashMap::new()),
            rebootstrap_last_failure: RwLock::new(HashMap::new()),
            rebootstrap_inflight: RwLock::new(HashMap::new()),
            local_orr_id,
            local_qkc_id,
        }
    }

    // ─── qkc_id / pubkey accessors (sin cambios respecto a v2) ────────

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

    /// Clave pública ML-KEM long-term del peer. `None` si no la
    /// conocemos. Solo se usa UNA VEZ en el bootstrap inicial (audit
    /// H-3: nunca más después).
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

    // ─── bootstrap_secret (HMAC key) ──────────────────────────────────

    /// Guarda el `bootstrap_secret` derivado del ML-KEM encap inicial
    /// contra la long-term pk del peer. **Solo se usará como clave
    /// HMAC**, jamás como keystream.
    pub fn set_bootstrap(&self, orr_id: String, secret: [u8; 32]) {
        self.bootstrap_secrets
            .write()
            .insert(orr_id, Zeroizing::new(secret));
    }

    /// Devuelve una **copia** del `bootstrap_secret` del peer (envuelta
    /// en `Zeroizing` para que se borre al drop del caller). `None` si
    /// el bootstrap inicial aún no completó con ese peer.
    pub fn bootstrap_for(&self, orr_id: &str) -> Option<Zeroizing<[u8; 32]>> {
        self.bootstrap_secrets
            .read()
            .get(orr_id)
            .map(|z| Zeroizing::new(**z))
    }

    pub fn has_bootstrap(&self, orr_id: &str) -> bool {
        self.bootstrap_secrets.read().contains_key(orr_id)
    }

    // ─── master_secrets[epoch] (keystream) ────────────────────────────

    /// Inserta el `master_secret` resultado de una rotación. `epoch_id`
    /// debe ser monotónicamente creciente (validación deferred al
    /// caller — el sub-protocolo de rotación lo garantiza).
    pub fn set_master_for_epoch(&self, orr_id: String, epoch_id: u32, secret: [u8; 32]) {
        self.master_secrets
            .write()
            .entry(orr_id)
            .or_default()
            .insert(epoch_id, Zeroizing::new(secret));
    }

    /// Devuelve una **copia** del `master_secret` para `(peer,
    /// epoch_id)`. `None` si esa época no fue rotada todavía (o ya fue
    /// dropeada por `drop_old_epochs`). El caller que pela un onion
    /// frame usa el `epoch_id` que viene en el wire (`Frame.epoch_id`).
    pub fn master_for_epoch(&self, orr_id: &str, epoch_id: u32) -> Option<[u8; 32]> {
        self.master_secrets
            .read()
            .get(orr_id)
            .and_then(|m| m.get(&epoch_id))
            .map(|z| **z)
    }

    /// Última época poblada para el peer (= máxima clave del BTreeMap).
    /// `None` si todavía no hay ninguna.
    pub fn latest_epoch_for(&self, orr_id: &str) -> Option<u32> {
        self.master_secrets
            .read()
            .get(orr_id)
            .and_then(|m| m.keys().next_back().copied())
    }

    /// Mantiene solo las últimas `keep_last_n` épocas en
    /// `master_secrets[peer]` y `ephemeral_sks[peer]`. Las demás se
    /// dropean (los `Zeroizing` se zeroizan al ser drop). `keep_last_n
    /// = 0` borra todo. Idempotente.
    pub fn drop_old_epochs(&self, keep_last_n: usize) {
        let mut master = self.master_secrets.write();
        for m in master.values_mut() {
            while m.len() > keep_last_n {
                // BTreeMap::pop_first elimina la entrada de menor clave.
                if m.pop_first().is_none() {
                    break;
                }
            }
        }
        let mut esks = self.ephemeral_sks.write();
        for e in esks.values_mut() {
            while e.len() > keep_last_n {
                if e.pop_first().is_none() {
                    break;
                }
            }
        }
    }

    // ─── Conveniencia: latest master_secret + has_master_secret ──────

    /// Devuelve una copia del `master_secret` de la última época
    /// poblada para el peer. Equivalente a
    /// `master_for_epoch(peer, latest_epoch_for(peer)?)`. Útil para
    /// `/healthz` y diagnóstico; los callsites de envío de onion en
    /// `service.rs` usan directamente `master_for_epoch` con la época
    /// resuelta vía `latest_epoch_for`.
    pub fn master_secret(&self, orr_id: &str) -> Option<[u8; 32]> {
        let latest = self.latest_epoch_for(orr_id)?;
        self.master_for_epoch(orr_id, latest)
    }

    /// Hay al menos una época poblada para el peer.
    pub fn has_master_secret(&self, orr_id: &str) -> bool {
        self.master_secrets
            .read()
            .get(orr_id)
            .is_some_and(|m| !m.is_empty())
    }

    /// Snapshot del set de peers con master_secret establecido. Útil
    /// para `/healthz` y diagnóstico de bootstrap incompleto.
    pub fn peers_with_secret(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .master_secrets
            .read()
            .iter()
            .filter_map(|(k, m)| if m.is_empty() { None } else { Some(k.clone()) })
            .collect();
        v.sort_unstable();
        v
    }

    // ─── ephemeral_sks (responder side) ───────────────────────────────

    /// Guarda la esk efímera generada en el handler de
    /// `RequestEphemeralKey` (lado responder de la rotación). Vive
    /// hasta que `take_ephemeral_sk` la consuma al recibir el
    /// `EstablishEphemeralSecret` correspondiente.
    pub fn store_ephemeral_sk(&self, orr_id: String, epoch_id: u32, esk: Vec<u8>) {
        self.ephemeral_sks
            .write()
            .entry(orr_id)
            .or_default()
            .insert(epoch_id, Zeroizing::new(esk));
    }

    /// **Consume y devuelve** la esk para `(peer, epoch_id)` — la
    /// retira del mapa. El caller la usa para decapsular el
    /// `EstablishEphemeralSecret.ciphertext` y, al droparla,
    /// `Zeroizing` la borra. Tras este punto la esk no existe en
    /// ningún sitio: **forward secrecy boundary**.
    pub fn take_ephemeral_sk(&self, orr_id: &str, epoch_id: u32) -> Option<Zeroizing<Vec<u8>>> {
        let mut esks = self.ephemeral_sks.write();
        let entry = esks.get_mut(orr_id)?;
        entry.remove(&epoch_id)
    }

    pub fn has_ephemeral_sk(&self, orr_id: &str, epoch_id: u32) -> bool {
        self.ephemeral_sks
            .read()
            .get(orr_id)
            .is_some_and(|m| m.contains_key(&epoch_id))
    }

    // ─── current_send_epoch (initiator side) ─────────────────────────

    pub fn current_send_epoch(&self, orr_id: &str) -> Option<u32> {
        self.current_send_epochs.read().get(orr_id).copied()
    }

    pub fn set_current_send_epoch(&self, orr_id: String, epoch_id: u32) {
        self.current_send_epochs.write().insert(orr_id, epoch_id);
    }

    // ─── passive re-bootstrap reactivo ────────────────────────────────
    //
    // Si un peer reinicia su pod (rolling restart, OOMKilled, etc.) su
    // `PeerRegistry` (memory-only) se vacía. Cuando intenta cifrar y
    // mandarnos frames con un master_secret nuevo, **nuestro** lado
    // sigue con el viejo y los frames son undecryptables. El bootstrap
    // inicial no se vuelve a disparar porque la convención lex-smaller
    // solo cubre el caso happy-path inicial.
    //
    // Solución: cuando `handle_onion_in` detecta `master_secret missing
    // for epoch` para un peer conocido, lanzamos un re-bootstrap
    // reactivo (ver `OrrService::trigger_passive_rebootstrap`). El
    // emisor (que somos nosotros aquí) hace `kem.encap(peer.pk)` y le
    // manda un `EstablishSecret` al peer; el peer, con el handler
    // arreglado (siempre decap + store), guarda el nuevo
    // `master_secret`. Ambos lados quedan re-sincronizados.
    //
    // Los métodos siguientes implementan el rate-limit + dedup
    // necesarios para evitar tormentas de re-handshakes.

    /// Borra **todo** el material secreto asociado al peer:
    /// `bootstrap_secret`, todas las épocas de `master_secrets` y
    /// `ephemeral_sks`, y la `current_send_epoch`. Los `Zeroizing` que
    /// salen del mapa borran la memoria al dropearse.
    ///
    /// Usado por el re-bootstrap reactivo para forzar que la siguiente
    /// `attempt_establish` parta de cero y la nueva clave sustituya por
    /// completo a la vieja.
    pub fn clear_peer_secrets(&self, orr_id: &str) {
        self.bootstrap_secrets.write().remove(orr_id);
        self.master_secrets.write().remove(orr_id);
        self.ephemeral_sks.write().remove(orr_id);
        self.current_send_epochs.write().remove(orr_id);
    }

    /// Marca un re-bootstrap como "en vuelo" para el peer. Devuelve
    /// `true` si el caller adquirió la exclusiva (= debe proceder con
    /// el handshake), `false` si ya había otro hilo dentro (= debe
    /// abortar y dropear el frame que lo disparó).
    ///
    /// Es la primitiva de dedup: si llegan 100 frames undecryptable de
    /// orr_X en 50 ms, solo el primero adquiere y los demás ven `false`.
    pub fn try_mark_rebootstrap_inflight(&self, orr_id: &str) -> bool {
        let mut w = self.rebootstrap_inflight.write();
        if w.get(orr_id).copied().unwrap_or(false) {
            return false;
        }
        w.insert(orr_id.to_string(), true);
        true
    }

    /// Libera el flag in-flight. Debe llamarse SIEMPRE (idealmente con
    /// un guard RAII en el caller; aquí lo dejamos explícito porque el
    /// callsite hace .spawn y la liberación va al final del task).
    pub fn clear_rebootstrap_inflight(&self, orr_id: &str) {
        self.rebootstrap_inflight.write().remove(orr_id);
    }

    /// Decide si vale la pena disparar un re-bootstrap reactivo para
    /// este peer. Aplica `min_interval` SOLO si el último intento FALLÓ
    /// recientemente; si nunca se intentó o el último fue exitoso, no
    /// hay rate-limit.
    ///
    /// Optimización 2026-05-25: antes el rate-limit aplicaba siempre,
    /// añadiendo 30 s de latencia incluso al primer intento. Con
    /// múltiples peers afectados secuencialmente eso podía acumular
    /// minutos de recovery time.
    pub fn should_attempt_rebootstrap(
        &self,
        orr_id: &str,
        min_interval: std::time::Duration,
    ) -> bool {
        let r = self.rebootstrap_last_failure.read();
        match r.get(orr_id) {
            None => true, // nunca falló (o se limpió tras éxito) → proceder
            Some(last) => last.elapsed() >= min_interval,
        }
    }

    /// Registra que el último intento de re-bootstrap para este peer
    /// FALLÓ. El próximo intento esperará `min_interval`. Llamar SOLO
    /// en la rama Err del task.
    pub fn mark_rebootstrap_failure(&self, orr_id: &str) {
        self.rebootstrap_last_failure
            .write()
            .insert(orr_id.to_string(), Instant::now());
    }

    /// Limpia el marcador de fallo previo. Llamar en la rama Ok del
    /// task: el próximo intento (si lo hay) procederá sin espera.
    pub fn clear_rebootstrap_failure(&self, orr_id: &str) {
        self.rebootstrap_last_failure.write().remove(orr_id);
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
    fn master_secret_query_helpers() {
        // El getter plano `master_secret(peer)` devuelve la última
        // época poblada (= `latest_epoch_for + master_for_epoch`).
        // `has_master_secret` reporta si hay al menos una.
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert!(reg.master_secret("orr_2").is_none());
        assert!(!reg.has_master_secret("orr_2"));
        let s = [0xCC; 32];
        reg.set_master_for_epoch("orr_2".into(), 5, s);
        assert_eq!(reg.master_secret("orr_2"), Some(s));
        assert!(reg.has_master_secret("orr_2"));
        assert_eq!(reg.peers_with_secret(), vec!["orr_2"]);
        assert_eq!(reg.master_for_epoch("orr_2", 5), Some(s));
        assert_eq!(reg.latest_epoch_for("orr_2"), Some(5));
    }

    #[test]
    fn bootstrap_secret_storage() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert!(!reg.has_bootstrap("orr_2"));
        assert!(reg.bootstrap_for("orr_2").is_none());
        reg.set_bootstrap("orr_2".into(), [0xDD; 32]);
        assert!(reg.has_bootstrap("orr_2"));
        // El Zeroizing<[u8;32]> permite Deref a [u8;32].
        let got = reg.bootstrap_for("orr_2").unwrap();
        assert_eq!(*got, [0xDD; 32]);
    }

    #[test]
    fn master_for_epoch_multiple_epochs() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        reg.set_master_for_epoch("orr_2".into(), 1, [0x11; 32]);
        reg.set_master_for_epoch("orr_2".into(), 2, [0x22; 32]);
        reg.set_master_for_epoch("orr_2".into(), 3, [0x33; 32]);
        assert_eq!(reg.master_for_epoch("orr_2", 1), Some([0x11; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 2), Some([0x22; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 3), Some([0x33; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 999), None);
        assert_eq!(reg.latest_epoch_for("orr_2"), Some(3));
        // El getter plano devuelve la latest.
        assert_eq!(reg.master_secret("orr_2"), Some([0x33; 32]));
    }

    #[test]
    fn drop_old_epochs_keeps_last_n() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        for e in 1..=5u32 {
            reg.set_master_for_epoch("orr_2".into(), e, [e as u8; 32]);
        }
        reg.drop_old_epochs(3);
        // Solo 3..=5 deben sobrevivir.
        assert!(reg.master_for_epoch("orr_2", 1).is_none());
        assert!(reg.master_for_epoch("orr_2", 2).is_none());
        assert_eq!(reg.master_for_epoch("orr_2", 3), Some([3; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 4), Some([4; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 5), Some([5; 32]));
        assert_eq!(reg.latest_epoch_for("orr_2"), Some(5));
    }

    #[test]
    fn drop_old_epochs_keep_zero_clears_all() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        reg.set_master_for_epoch("orr_2".into(), 1, [0x11; 32]);
        reg.drop_old_epochs(0);
        assert!(!reg.has_master_secret("orr_2"));
    }

    #[test]
    fn drop_old_epochs_no_op_when_within_budget() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        reg.set_master_for_epoch("orr_2".into(), 1, [0x11; 32]);
        reg.set_master_for_epoch("orr_2".into(), 2, [0x22; 32]);
        reg.drop_old_epochs(10);
        // Ambas siguen.
        assert_eq!(reg.master_for_epoch("orr_2", 1), Some([0x11; 32]));
        assert_eq!(reg.master_for_epoch("orr_2", 2), Some([0x22; 32]));
    }

    #[test]
    fn ephemeral_sk_store_take_zeroizes() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert!(!reg.has_ephemeral_sk("orr_2", 1));
        let esk = vec![0xEE; 2400]; // size de ML-KEM-768 sk
        reg.store_ephemeral_sk("orr_2".into(), 1, esk.clone());
        assert!(reg.has_ephemeral_sk("orr_2", 1));
        let taken = reg.take_ephemeral_sk("orr_2", 1).expect("present");
        assert_eq!(&taken[..], &esk[..]);
        // Tras take, la esk ya no está en el mapa.
        assert!(!reg.has_ephemeral_sk("orr_2", 1));
        // Y un segundo take devuelve None.
        assert!(reg.take_ephemeral_sk("orr_2", 1).is_none());
        // Al dropear `taken`, Zeroizing borra los bytes.
        drop(taken);
    }

    #[test]
    fn drop_old_epochs_evicts_ephemeral_sks_too() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        for e in 1..=5u32 {
            reg.store_ephemeral_sk("orr_2".into(), e, vec![e as u8; 8]);
        }
        reg.drop_old_epochs(2);
        // Solo 4..=5 deben sobrevivir.
        assert!(!reg.has_ephemeral_sk("orr_2", 1));
        assert!(!reg.has_ephemeral_sk("orr_2", 2));
        assert!(!reg.has_ephemeral_sk("orr_2", 3));
        assert!(reg.has_ephemeral_sk("orr_2", 4));
        assert!(reg.has_ephemeral_sk("orr_2", 5));
    }

    #[test]
    fn current_send_epoch_round_trip() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert_eq!(reg.current_send_epoch("orr_2"), None);
        reg.set_current_send_epoch("orr_2".into(), 5);
        assert_eq!(reg.current_send_epoch("orr_2"), Some(5));
        reg.set_current_send_epoch("orr_2".into(), 6);
        assert_eq!(reg.current_send_epoch("orr_2"), Some(6));
    }

    #[test]
    fn clear_peer_secrets_removes_everything() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        reg.set_bootstrap("orr_2".into(), [0xAA; 32]);
        reg.set_master_for_epoch("orr_2".into(), 0, [0xBB; 32]);
        reg.set_master_for_epoch("orr_2".into(), 1, [0xCC; 32]);
        reg.store_ephemeral_sk("orr_2".into(), 1, vec![0xDD; 8]);
        reg.set_current_send_epoch("orr_2".into(), 7);
        assert!(reg.has_bootstrap("orr_2"));
        assert!(reg.has_master_secret("orr_2"));
        assert!(reg.has_ephemeral_sk("orr_2", 1));

        reg.clear_peer_secrets("orr_2");

        assert!(!reg.has_bootstrap("orr_2"));
        assert!(!reg.has_master_secret("orr_2"));
        assert!(!reg.has_ephemeral_sk("orr_2", 1));
        assert_eq!(reg.current_send_epoch("orr_2"), None);
        // qkc_id y pubkey son metadata pública, NO se borran
        // (no es el job de clear_peer_secrets).
    }

    #[test]
    fn rebootstrap_inflight_dedup() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        assert!(reg.try_mark_rebootstrap_inflight("orr_2"));
        // Segundo intento concurrente debe fallar
        assert!(!reg.try_mark_rebootstrap_inflight("orr_2"));
        // Tras liberar, el siguiente puede entrar
        reg.clear_rebootstrap_inflight("orr_2");
        assert!(reg.try_mark_rebootstrap_inflight("orr_2"));
    }

    #[test]
    fn rebootstrap_rate_limit_blocks_only_after_failure() {
        use std::time::Duration;
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        // Sin historial → siempre OK (primer intento no espera)
        assert!(reg.should_attempt_rebootstrap("orr_2", Duration::from_secs(30)));
        // Marcar fallo → rate-limited dentro de la ventana
        reg.mark_rebootstrap_failure("orr_2");
        assert!(!reg.should_attempt_rebootstrap("orr_2", Duration::from_secs(30)));
        // Ventana 0 ns → siempre OK
        assert!(reg.should_attempt_rebootstrap("orr_2", Duration::from_nanos(0)));
        // Limpiar marca de fallo (= intento previo exitoso) → libre de nuevo
        reg.clear_rebootstrap_failure("orr_2");
        assert!(reg.should_attempt_rebootstrap("orr_2", Duration::from_secs(30)));
    }
}
