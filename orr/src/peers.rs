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
//!     Consumido por `onion::layer_key` como `ikm` del HKDF.
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
    /// orr_id → URL gRPC del peer. Se siembra con `cfg.peer_grpc_addrs` y
    /// la refresca el anunciador con lo que manda la SDN.
    ///
    /// Antes esto sólo existía en `cfg.peer_grpc_addrs` (inmutable), y el
    /// re-bootstrap pasivo lo usaba como filtro anti-flood. Con peers que
    /// llegan de la SDN ese filtro los rechazaba a todos: un peer aprendido
    /// dinámicamente que perdiera su `master_secret` (reinicio, rotación a
    /// medias) quedaba condenado a dropear frames para siempre, en
    /// silencio. Que es exactamente el síntoma "emito y no llega nada".
    grpc_addrs: RwLock<HashMap<String, String>>,
    /// Snapshot **inmutable** de los pins de `peer_pubkeys` (TOML). Se compara
    /// contra lo que llega por `GetPublicKey` para detectar/rechazar MITM
    /// (§Fase 6). Separado de `pubkeys`, que el fetch/rebootstrap sobrescribe.
    pinned_pubkeys: HashMap<String, Vec<u8>>,
    /// `orr_id -> ML-DSA verifying key` de los peers (§Fase 6 PQC), para
    /// verificar la firma de su anuncio de pubkey. Inmutable (de config).
    peer_verify_keys: HashMap<String, Vec<u8>>,
    bootstrap_trust: crate::config::BootstrapTrust,
    /// CA de red, para verificar la cadena de un anuncio firmado con la clave
    /// del cert de nodo (`signing_certs`). `None` sin `[tls]`.
    trust_roots: Option<common::cert_identity::TrustRoots>,
    /// Pares para los que ya hay una task de rotación viva: una por par,
    /// arrancada al terminar el bootstrap del lado iniciador.
    rotation_spawned: RwLock<std::collections::HashSet<String>>,
    local_orr_id: String,
    local_qkc_id: u32,
}

/// Veredicto sobre la **firma ML-DSA** del anuncio de pubkey de un peer.
#[derive(Debug, PartialEq, Eq)]
pub enum SigVerdict {
    /// Firma válida con la clave del **cert de nodo** del peer, cuya cadena
    /// verifica contra la CA de red y cuyo SAN es `dkms://<orr_id>`. No
    /// necesita config por par y sobrevive a los reinicios del peer.
    CertBound,
    /// Firma válida contra la verify key configurada del peer (heredado).
    Valid,
    /// No hay verify key configurada para este peer (tofu: se acepta sin firma).
    NoKey,
    /// Hay verify key pero el anuncio no traía firma (tofu: se avisa).
    Unsigned,
    /// Rechazar: firma inválida, o strict sin verify key / sin firma.
    Reject,
}

/// Veredicto sobre una pubkey recién obtenida por `GetPublicKey`.
#[derive(Debug, PartialEq, Eq)]
pub enum PubkeyVerdict {
    /// Aceptar y almacenar (no hay pin, o casa el pin).
    Accept,
    /// TOFU: aceptar, pero difiere del pin configurado — posible reinicio del
    /// peer (identidad efímera) o MITM. Se registra un aviso.
    AcceptPinMismatch,
    /// `strict`: rechazar (no casa ningún pin configurado).
    Reject,
}

/// Tope de `esk` pendientes por par en el lado responder (B9): un peer que
/// manda `RequestEphemeralKey` con `epoch_id` crecientes hacía crecer
/// `ephemeral_sks[peer]` sin límite (+ un keygen ML-KEM por REQ). Una rotación
/// legítima usa una a la vez; 64 deja margen de solape.
const MAX_EPHEMERAL_PER_PEER: usize = 64;

impl PeerRegistry {
    pub fn new(seed_qkc: HashMap<String, u32>, local_orr_id: String, local_qkc_id: u32) -> Self {
        Self::with_pubkeys(seed_qkc, HashMap::new(), local_orr_id, local_qkc_id)
    }

    /// Constructor con ambos sembrados a la vez. Lo usa `OrrService::new`
    /// para inyectar los pubkeys decodificados desde TOML. `bootstrap_trust`
    /// = Tofu (default) — usa [`Self::with_pubkeys_trust`] para fijarlo.
    pub fn with_pubkeys(
        seed_qkc: HashMap<String, u32>,
        seed_pubkeys: HashMap<String, Vec<u8>>,
        local_orr_id: String,
        local_qkc_id: u32,
    ) -> Self {
        Self::with_pubkeys_trust(
            seed_qkc,
            seed_pubkeys,
            local_orr_id,
            local_qkc_id,
            crate::config::BootstrapTrust::default(),
        )
    }

    /// Como [`Self::with_pubkeys`] fijando la política de confianza. Los pins
    /// (`seed_pubkeys`) se guardan también como snapshot inmutable.
    pub fn with_pubkeys_trust(
        seed_qkc: HashMap<String, u32>,
        seed_pubkeys: HashMap<String, Vec<u8>>,
        local_orr_id: String,
        local_qkc_id: u32,
        bootstrap_trust: crate::config::BootstrapTrust,
    ) -> Self {
        Self::with_all(
            seed_qkc,
            seed_pubkeys,
            HashMap::new(),
            local_orr_id,
            local_qkc_id,
            bootstrap_trust,
        )
    }

    /// CA de red con la que verificar los anuncios atados al cert de nodo.
    pub fn with_trust_roots(mut self, roots: Option<common::cert_identity::TrustRoots>) -> Self {
        self.trust_roots = roots;
        self
    }

    /// Constructor completo: además de los pins, las **verifying keys ML-DSA**
    /// de los peers (§Fase 6 PQC) para verificar la firma de sus anuncios.
    pub fn with_all(
        seed_qkc: HashMap<String, u32>,
        seed_pubkeys: HashMap<String, Vec<u8>>,
        peer_verify_keys: HashMap<String, Vec<u8>>,
        local_orr_id: String,
        local_qkc_id: u32,
        bootstrap_trust: crate::config::BootstrapTrust,
    ) -> Self {
        Self {
            by_orr: RwLock::new(seed_qkc),
            pinned_pubkeys: seed_pubkeys.clone(),
            peer_verify_keys,
            pubkeys: RwLock::new(seed_pubkeys),
            bootstrap_secrets: RwLock::new(HashMap::new()),
            master_secrets: RwLock::new(HashMap::new()),
            ephemeral_sks: RwLock::new(HashMap::new()),
            current_send_epochs: RwLock::new(HashMap::new()),
            rebootstrap_last_failure: RwLock::new(HashMap::new()),
            rebootstrap_inflight: RwLock::new(HashMap::new()),
            grpc_addrs: RwLock::new(HashMap::new()),
            bootstrap_trust,
            trust_roots: None,
            rotation_spawned: RwLock::new(std::collections::HashSet::new()),
            local_orr_id,
            local_qkc_id,
        }
    }

    /// Decide si aceptar una pubkey obtenida por `GetPublicKey`, según la
    /// política `bootstrap_trust` y el pin de `peer_pubkeys` (si lo hay).
    /// Función pura (testeable); el caller registra el aviso y actúa.
    pub fn verify_fetched_pubkey(&self, orr_id: &str, fetched: &[u8]) -> PubkeyVerdict {
        use crate::config::BootstrapTrust::*;
        let pin = self.pinned_pubkeys.get(orr_id);
        match (self.bootstrap_trust, pin) {
            // strict: solo si casa un pin configurado.
            (Strict, Some(p)) if p.as_slice() == fetched => PubkeyVerdict::Accept,
            (Strict, _) => PubkeyVerdict::Reject,
            // tofu: acepta siempre; avisa si difiere de un pin (reinicio/MITM).
            (Tofu, Some(p)) if p.as_slice() != fetched => PubkeyVerdict::AcceptPinMismatch,
            (Tofu, _) => PubkeyVerdict::Accept,
        }
    }

    /// Verifica la **firma ML-DSA** del anuncio de pubkey de un peer
    /// (§Fase 6 PQC). Devuelve el veredicto según haya verify key configurada,
    /// la firma valide, y la política `bootstrap_trust`.
    pub fn verify_announcement(
        &self,
        orr_id: &str,
        suite: &str,
        public_key: &[u8],
        signature: &[u8],
        signing_certs: &[Vec<u8>],
    ) -> SigVerdict {
        use crate::config::BootstrapTrust::*;
        // Anuncio atado al cert de nodo (desde 2026-08-30): la cadena tiene
        // que encadenar hasta la CA de red y el SAN tiene que ser este
        // orr_id; la firma se verifica con la clave de ese cert. Una firma
        // inválida o una cadena ajena se rechaza siempre, en tofu también:
        // alguien está intentando algo.
        if !signing_certs.is_empty() {
            let Some(roots) = &self.trust_roots else {
                tracing::warn!(
                    peer = %orr_id,
                    "orr.peer_pubkey: anuncio firmado con cert de nodo pero este ORR no tiene \
                     CA de red cargada ([tls] ausente); no puedo verificar la cadena"
                );
                return match self.bootstrap_trust {
                    Strict => SigVerdict::Reject,
                    Tofu => SigVerdict::Unsigned,
                };
            };
            return match common::cert_identity::verify_node_cert(signing_certs, roots, orr_id) {
                Ok(vk) => match common::crypto::pqc_sign::verify_orr_pubkey(
                    &vk, orr_id, suite, public_key, signature,
                ) {
                    Ok(()) => SigVerdict::CertBound,
                    Err(_) => SigVerdict::Reject,
                },
                Err(e) => {
                    tracing::warn!(
                        peer = %orr_id,
                        error = %e,
                        "orr.peer_pubkey: cadena del anuncio rechazada"
                    );
                    SigVerdict::Reject
                }
            };
        }
        match self.peer_verify_keys.get(orr_id) {
            Some(vk) => {
                if signature.is_empty() {
                    // Tenemos su clave pero no vino firma: en strict se rechaza.
                    return match self.bootstrap_trust {
                        Strict => SigVerdict::Reject,
                        Tofu => SigVerdict::Unsigned,
                    };
                }
                match common::crypto::pqc_sign::verify_orr_pubkey(
                    vk, orr_id, suite, public_key, signature,
                ) {
                    Ok(()) => SigVerdict::Valid,
                    Err(_) => SigVerdict::Reject, // firma inválida: siempre se rechaza
                }
            }
            // Sin verify key configurada: no podemos verificar. strict lo exige.
            None => match self.bootstrap_trust {
                Strict => SigVerdict::Reject,
                Tofu => SigVerdict::NoKey,
            },
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

    /// Olvida a un peer **por completo**: su qkc_id, su pubkey y todo su
    /// material criptográfico.
    ///
    /// Lo usa el anunciador cuando la SDN deja de listar a un peer. Quitarlo
    /// solo de `by_orr` (que es lo que hace [`Self::remove`]) dejaría vivos su
    /// `bootstrap_secret` y sus `master_secrets` hasta que muriera el proceso.
    /// Todos son `Zeroizing`, así que al soltarlos se borran de memoria.
    pub fn forget(&self, orr_id: &str) {
        self.by_orr.write().remove(orr_id);
        self.pubkeys.write().remove(orr_id);
        self.bootstrap_secrets.write().remove(orr_id);
        self.master_secrets.write().remove(orr_id);
        self.ephemeral_sks.write().remove(orr_id);
        self.current_send_epochs.write().remove(orr_id);
        self.rebootstrap_last_failure.write().remove(orr_id);
        self.rebootstrap_inflight.write().remove(orr_id);
        self.grpc_addrs.write().remove(orr_id);
        self.rotation_spawned.write().remove(orr_id);
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

    /// URL gRPC del peer, venga del `node.yml` o de la SDN. `None` = no lo
    /// conocemos por ninguna vía, y entonces no hay a dónde re-bootstrapear.
    pub fn grpc_addr(&self, orr_id: &str) -> Option<String> {
        self.grpc_addrs.read().get(orr_id).cloned()
    }

    pub fn put_grpc_addr(&self, orr_id: String, url: String) {
        self.grpc_addrs.write().insert(orr_id, url);
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
        let mut esks = self.ephemeral_sks.write();
        let m = esks.entry(orr_id).or_default();
        m.insert(epoch_id, Zeroizing::new(esk));
        // Cota por par (B9): conserva solo las últimas MAX_EPHEMERAL_PER_PEER
        // épocas; las más viejas se dropean (los `Zeroizing` se zeroizan). Sin
        // esto, un peer que inunda REQ con épocas crecientes crecía sin límite.
        while m.len() > MAX_EPHEMERAL_PER_PEER {
            let oldest = *m.keys().next().expect("len > cap ⇒ no vacío");
            m.remove(&oldest);
        }
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

    /// Época con la que ENVIAR a `orr_id`. En el iniciador es la última que el
    /// respondedor confirmó con su `ok` al FIN (`current_send_epoch`); en el
    /// respondedor, que no negocia, la última instalada. Nunca una época que
    /// el otro extremo pueda no tener todavía: el iniciador guarda la nueva
    /// ANTES de mandar el FIN, así que lo que el respondedor instala al
    /// recibirlo ya existe aquí (ver `rotation::run_one_rotation`).
    pub fn send_epoch_for(&self, orr_id: &str) -> Option<u32> {
        self.current_send_epoch(orr_id)
            .or_else(|| self.latest_epoch_for(orr_id))
    }

    /// Quita UNA época: la provisional de una rotación cuyo FIN no llegó a
    /// confirmarse. No toca la `current_send_epoch`.
    pub fn remove_master_epoch(&self, orr_id: &str, epoch_id: u32) {
        if let Some(m) = self.master_secrets.write().get_mut(orr_id) {
            m.remove(&epoch_id);
        }
    }

    /// Un bootstrap —inicial o rehecho— sustituye TODA la historia con el
    /// peer: el `bootstrap_secret` nuevo, la época 0 sembrada con él, y ni
    /// una época ni una esk de antes. Quien rehace el bootstrap ha perdido su
    /// estado (o ha visto que el otro lo perdió): seguir cifrando con épocas
    /// que el otro no tiene es justo lo que dejaba los frames en «missing
    /// epoch». La `current_send_epoch` se borra: el envío cae a la 0 y la
    /// primera rotación vuelve a empezar en la 1. Espejo del «prune after
    /// relink» del QKC.
    pub fn reset_for_bootstrap(&self, orr_id: &str, secret: [u8; 32]) {
        self.master_secrets.write().remove(orr_id);
        self.ephemeral_sks.write().remove(orr_id);
        self.current_send_epochs.write().remove(orr_id);
        self.bootstrap_secrets
            .write()
            .insert(orr_id.to_string(), Zeroizing::new(secret));
        self.set_master_for_epoch(orr_id.to_string(), 0, secret);
    }

    /// [`Self::drop_old_epochs`] para UN peer: lo llama cada extremo tras
    /// instalar una época. `keep_last_n` se acota a ≥ 2 porque entre que el
    /// respondedor instala N y el iniciador recibe el `ok`, el iniciador
    /// sigue cifrando con N−1.
    pub fn drop_old_epochs_for(&self, orr_id: &str, keep_last_n: usize) {
        let keep = keep_last_n.max(2);
        if let Some(m) = self.master_secrets.write().get_mut(orr_id) {
            while m.len() > keep {
                if m.pop_first().is_none() {
                    break;
                }
            }
        }
        if let Some(e) = self.ephemeral_sks.write().get_mut(orr_id) {
            while e.len() > keep {
                if e.pop_first().is_none() {
                    break;
                }
            }
        }
    }

    /// `true` la primera vez que se pide para `orr_id`: quien lo obtiene
    /// arranca la task de rotación de ese par, y nadie más. `forget` lo
    /// libera.
    pub fn try_mark_rotation_spawned(&self, orr_id: &str) -> bool {
        self.rotation_spawned.write().insert(orr_id.to_string())
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
    fn verify_announcement_signature() {
        use crate::config::BootstrapTrust::*;
        use common::crypto::pqc_sign;
        let kp = pqc_sign::keygen();
        let pk = vec![0x33; 1184];
        let sig = pqc_sign::sign_orr_pubkey(&kp.secret_seed, "orr_2", "ml-kem-768", &pk).unwrap();

        let mut vks = HashMap::new();
        vks.insert("orr_2".to_string(), kp.verifying_key.clone());

        // Con verify key configurada: firma válida → Valid; inválida/pubkey
        // sustituida → Reject.
        let reg = PeerRegistry::with_all(
            HashMap::new(),
            HashMap::new(),
            vks.clone(),
            "orr_1".into(),
            1,
            Tofu,
        );
        assert_eq!(
            reg.verify_announcement("orr_2", "ml-kem-768", &pk, &sig, &[]),
            SigVerdict::Valid
        );
        assert_eq!(
            reg.verify_announcement("orr_2", "ml-kem-768", &[0x44; 1184], &sig, &[]),
            SigVerdict::Reject,
            "un MITM que sustituye la pubkey no puede reproducir la firma",
        );
        // Sin firma, tofu → Unsigned; strict → Reject.
        assert_eq!(
            reg.verify_announcement("orr_2", "ml-kem-768", &pk, &[], &[]),
            SigVerdict::Unsigned
        );
        let reg_strict = PeerRegistry::with_all(
            HashMap::new(),
            HashMap::new(),
            vks,
            "orr_1".into(),
            1,
            Strict,
        );
        assert_eq!(
            reg_strict.verify_announcement("orr_2", "ml-kem-768", &pk, &[], &[]),
            SigVerdict::Reject
        );

        // Sin verify key: tofu → NoKey; strict → Reject.
        let reg_nokey = PeerRegistry::with_all(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            "orr_1".into(),
            1,
            Tofu,
        );
        assert_eq!(
            reg_nokey.verify_announcement("orr_9", "ml-kem-768", &pk, &sig, &[]),
            SigVerdict::NoKey
        );
        let reg_nokey_strict = PeerRegistry::with_all(
            HashMap::new(),
            HashMap::new(),
            HashMap::new(),
            "orr_1".into(),
            1,
            Strict,
        );
        assert_eq!(
            reg_nokey_strict.verify_announcement("orr_9", "ml-kem-768", &pk, &sig, &[]),
            SigVerdict::Reject
        );
    }

    #[test]
    fn verify_fetched_pubkey_tofu_and_strict() {
        use crate::config::BootstrapTrust::*;
        let mut q = HashMap::new();
        q.insert("orr_2".into(), 2);
        let mut pins = HashMap::new();
        pins.insert("orr_2".into(), vec![0xAA; 8]);

        // tofu (default): acepta todo; avisa si difiere del pin.
        let tofu =
            PeerRegistry::with_pubkeys_trust(q.clone(), pins.clone(), "orr_1".into(), 1, Tofu);
        assert_eq!(
            tofu.verify_fetched_pubkey("orr_2", &[0xAA; 8]),
            PubkeyVerdict::Accept
        );
        assert_eq!(
            tofu.verify_fetched_pubkey("orr_2", &[0xBB; 8]),
            PubkeyVerdict::AcceptPinMismatch,
        );
        // sin pin: tofu acepta (TOFU).
        assert_eq!(
            tofu.verify_fetched_pubkey("orr_9", &[1, 2, 3]),
            PubkeyVerdict::Accept
        );

        // strict: solo si casa un pin.
        let strict = PeerRegistry::with_pubkeys_trust(q, pins, "orr_1".into(), 1, Strict);
        assert_eq!(
            strict.verify_fetched_pubkey("orr_2", &[0xAA; 8]),
            PubkeyVerdict::Accept
        );
        assert_eq!(
            strict.verify_fetched_pubkey("orr_2", &[0xBB; 8]),
            PubkeyVerdict::Reject
        );
        assert_eq!(
            strict.verify_fetched_pubkey("orr_9", &[1, 2, 3]),
            PubkeyVerdict::Reject
        );
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
    fn store_ephemeral_sk_is_capped_per_peer() {
        let reg = PeerRegistry::new(HashMap::new(), "orr_1".into(), 1);
        // Un flood de épocas crecientes: solo sobreviven las últimas del cap.
        for e in 1..=(MAX_EPHEMERAL_PER_PEER as u32 + 50) {
            reg.store_ephemeral_sk("orr_2".into(), e, vec![0u8; 8]);
        }
        // La más vieja (época 1) fue evictada; la última sigue.
        assert!(!reg.has_ephemeral_sk("orr_2", 1));
        assert!(reg.has_ephemeral_sk("orr_2", MAX_EPHEMERAL_PER_PEER as u32 + 50));
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

    /// El anuncio firmado con la clave del cert de nodo se verifica contra la
    /// CA de red y el SAN, sin `peer_verify_keys`: `strict` funciona con
    /// peers que nadie configuró a mano, y el ancla sobrevive a los reinicios
    /// de la identidad ML-KEM.
    #[test]
    fn a_cert_bound_announcement_verifies_against_the_network_ca_needs_openssl35() {
        use crate::config::BootstrapTrust;
        use common::cert_identity::TrustRoots;
        use common::crypto::pqc_sign::{sign_orr_pubkey_with, MlDsa65Signer};

        let dir = std::env::temp_dir().join(format!("orr_peers_pki_{}", std::process::id()));
        let Some(pki) = common::test_support::mldsa_test_pki(&dir, &["orr_1", "orr_2"]) else {
            common::test_support::skip_or_fail("openssl sin ML-DSA (<3.5)");
            return;
        };
        let strict = |roots: Option<TrustRoots>| {
            PeerRegistry::with_all(
                HashMap::new(),
                HashMap::new(),
                HashMap::new(),
                "orr_9".into(),
                9,
                BootstrapTrust::Strict,
            )
            .with_trust_roots(roots)
        };
        let reg = strict(Some(TrustRoots::from_file(&pki.ca_crt).unwrap()));
        let signer =
            MlDsa65Signer::from_pkcs8_pem(&std::fs::read(pki.key("orr_1")).unwrap()).unwrap();
        let pk = vec![0x11u8; 1184];
        let sig = sign_orr_pubkey_with(&signer, "orr_1", "ml-kem-768", &pk);
        let chain = vec![pki.cert_der("orr_1")];
        assert_eq!(
            reg.verify_announcement("orr_1", "ml-kem-768", &pk, &sig, &chain),
            SigVerdict::CertBound
        );
        // Reclamar ser orr_2 con el cert de orr_1: el SAN no casa.
        let sig2 = sign_orr_pubkey_with(&signer, "orr_2", "ml-kem-768", &pk);
        assert_eq!(
            reg.verify_announcement("orr_2", "ml-kem-768", &pk, &sig2, &chain),
            SigVerdict::Reject
        );
        // Pubkey alterada en tránsito.
        assert_eq!(
            reg.verify_announcement("orr_1", "ml-kem-768", &[0x12u8; 1184], &sig, &chain),
            SigVerdict::Reject
        );
        // Un cert perfecto de OTRA CA no vale.
        let rogue_dir =
            std::env::temp_dir().join(format!("orr_peers_rogue_{}", std::process::id()));
        let rogue = common::test_support::mldsa_test_pki(&rogue_dir, &["orr_1"]).unwrap();
        let rogue_signer =
            MlDsa65Signer::from_pkcs8_pem(&std::fs::read(rogue.key("orr_1")).unwrap()).unwrap();
        let rsig = sign_orr_pubkey_with(&rogue_signer, "orr_1", "ml-kem-768", &pk);
        assert_eq!(
            reg.verify_announcement(
                "orr_1",
                "ml-kem-768",
                &pk,
                &rsig,
                &[rogue.cert_der("orr_1")]
            ),
            SigVerdict::Reject
        );
        // Sin CA cargada, strict no puede verificar la cadena: rechaza.
        assert_eq!(
            strict(None).verify_announcement("orr_1", "ml-kem-768", &pk, &sig, &chain),
            SigVerdict::Reject
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&rogue_dir);
    }
}
