//! Fuente de claves PQC para un enlace QKC↔QKC (canal `link_type = "pqc"`).
//!
//! Imita el feed de claves del quditto (QKD) sin red ni quditto: los dos
//! QKC del enlace comparten un secreto de 32 B acordado por ML-KEM (ver
//! [`crate::pqc_handshake`]) y derivan de él, de forma **determinista**, el
//! material OTP de cada clave:
//!
//! ```text
//! K = HKDF-SHA256(salt = b"qkc.pqc.v1",
//!                 ikm  = secret_32B,
//!                 info = key_id ‖ u32_be(len) ‖ u32_be(chunk_idx),
//!                 L    = key_size_bits/8)
//! ```
//!
//! **Re-keying por épocas (forward secrecy).** En vez de un único secreto para
//! toda la vida del enlace, el secreto se **rota** periódicamente: cada "época"
//! tiene su propio secreto ML-KEM independiente. La época de cada clave viaja
//! **dentro del propio `key_id`** (sin tocar el wire de `FRAME_KEY_IDS_NOTIFY`):
//!
//! ```text
//! key_id = época (4 B big-endian) ‖ aleatorio (12 B)   // 16 B opacos
//! ```
//!
//! El emisor usa `época = store.highest() − lookahead` (las épocas
//! `lookahead` por delante se pre-cargan en background → la rotación es
//! transparente, sin latencia). El receptor lee la época de `key_id[0..4]`,
//! espera a que su [`SecretStore`] tenga ese secreto y deriva. Como la época
//! es auto-descriptiva, **no hacen falta contadores sincronizados** entre
//! extremos. Las épocas viejas se **zeroizan** al evictar (forward secrecy).
//!
//! El salt `b"qkc.pqc.v1"` es distinto del del onion de ORR
//! (`b"orr.onion.v1"`) para que las dos derivaciones nunca coincidan.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use hkdf::Hkdf;
use parking_lot::Mutex;
use sha2::Sha256;
use tokio::sync::Notify;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    error::{QkcError, Result},
    kme::{KeySource, OtpKey},
};

const QKC_PQC_SALT: &[u8] = b"qkc.pqc.v1";
/// Techo de salida de un solo `HKDF-SHA256::expand` (RFC 5869: 255·32 B).
const HKDF_MAX_OUTPUT: usize = 255 * 32;
/// Cuánto espera `enc/dec_keys` a que el secreto de una época esté listo
/// antes de devolver timeout. El `enc_refill_loop` reintenta tras su backoff.
const SECRET_WAIT: Duration = Duration::from_secs(10);

/// Deriva `len` bytes deterministas a partir de `(secret, key_id)` con
/// HKDF-SHA256. Soporta `len` arbitrario vía chunking (cada chunk usa un
/// `chunk_idx` distinto en el `info`). Ambos extremos del enlace derivan
/// bytes idénticos para el mismo `(secret, key_id, len)`. La época queda
/// aislada doblemente: selecciona el secreto Y va en los primeros 4 B del
/// `key_id` (que entra en el `info`).
pub fn derive_material(secret: &[u8; 32], key_id: &[u8; 16], len: usize) -> Vec<u8> {
    let hk = Hkdf::<Sha256>::new(Some(QKC_PQC_SALT), secret);
    let mut okm = vec![0u8; len];
    let mut info = [0u8; 16 + 4 + 4]; // key_id ‖ u32_be(len) ‖ u32_be(chunk_idx)
    info[..16].copy_from_slice(key_id);
    info[16..20].copy_from_slice(&(len as u32).to_be_bytes());

    let mut off = 0usize;
    let mut chunk_idx: u32 = 0;
    while off < len {
        let take = (len - off).min(HKDF_MAX_OUTPUT);
        info[20..24].copy_from_slice(&chunk_idx.to_be_bytes());
        hk.expand(&info, &mut okm[off..off + take])
            .expect("HKDF-SHA256 expand within 8160 B chunk");
        off += take;
        chunk_idx = chunk_idx
            .checked_add(1)
            .expect("len overflows chunk_idx u32");
    }
    okm
}

/// Época de un `key_id` (sus 4 primeros bytes, big-endian).
#[inline]
pub fn epoch_of(key_id: &[u8; 16]) -> u32 {
    u32::from_be_bytes([key_id[0], key_id[1], key_id[2], key_id[3]])
}

/// Construye un `key_id = época (4 B BE) ‖ aleatorio (12 B)`. El aleatorio
/// viene de un UUID v4 (≈122 b), suficiente para unicidad por época.
fn make_key_id(epoch: u32) -> Uuid {
    let mut b = *Uuid::new_v4().as_bytes();
    b[0..4].copy_from_slice(&epoch.to_be_bytes());
    Uuid::from_bytes(b)
}

fn timeout_err() -> QkcError {
    QkcError::KeyWaitTimeout {
        what: "pqc-secret",
        missing: 1,
        ms: SECRET_WAIT.as_millis() as u64,
    }
}

// ─────────────────────────── SecretStore ────────────────────────────────

/// Almacén compartido de secretos por época de UN enlace PQC. Lo rellena
/// [`crate::pqc_handshake`] (un ML-KEM por época) y lo consume
/// [`PqcKeySource`]. Mantiene una ventana deslizante; evicta (y zeroiza) las
/// épocas más viejas.
pub struct SecretStore {
    inner: Mutex<BTreeMap<u32, Zeroizing<[u8; 32]>>>,
    /// Nº máximo de épocas vivas (ventana deslizante).
    keep: usize,
    notify: Notify,
    /// Época más alta que el peer ha usado y que este extremo **no tiene**,
    /// o 0 si no hay nada pendiente. La levanta el lado DEC al descartar
    /// claves indescifrables y la consume el bucle de rotación del
    /// iniciador, que es el único que puede renegociar.
    ///
    /// Existe porque las dos ventanas de épocas pueden separarse sin que
    /// ninguno de los dos lo sepa: cada extremo calcula su época activa a
    /// partir de SU propio store, y un reinicio deja al que arranca con una
    /// ventana baja y al otro con la suya alta. El que arranca no puede
    /// adivinar la del peer — pero cada frame que recibe se la dice, en los
    /// 4 primeros bytes del `key_id`.
    resync_epoch: AtomicU32,
    resync: Notify,
}

impl SecretStore {
    /// `lookahead` = épocas pre-cargadas por delante; `rekey_keys` = N (claves
    /// por época). La ventana `keep` debe cubrir TODAS las épocas que pueda
    /// abarcar el buffer ENC (capacidad `BUFFER_TARGET*2`): si N es pequeño, el
    /// buffer guarda claves de muchas épocas y un `dec` de una clave aún en
    /// buffer fallaría si su secreto se evictó. ⇒ `keep ≥ lookahead + activa +
    /// ⌈cap_buffer / N⌉ + margen`.
    pub fn new(lookahead: u32, rekey_keys: u64) -> Arc<Self> {
        let enc_cap = (crate::keystore::BUFFER_TARGET * 2) as u64; // ArrayQueue cap
        let span = if rekey_keys == 0 {
            1
        } else {
            enc_cap.div_ceil(rekey_keys) as usize + 2
        };
        Arc::new(Self {
            inner: Mutex::new(BTreeMap::new()),
            keep: lookahead as usize + 1 + span,
            notify: Notify::new(),
            resync_epoch: AtomicU32::new(0),
            resync: Notify::new(),
        })
    }

    /// Avisa de que el peer está cifrando con `epoch`, que aquí no existe.
    ///
    /// Se queda con la más alta vista: el bloque que negocie el iniciador
    /// tiene que quedar por encima de la ventana del peer, o este seguirá
    /// usando la suya y no converge.
    pub fn request_resync(&self, epoch: u32) {
        self.resync_epoch.fetch_max(epoch, Ordering::Relaxed);
        self.resync.notify_one();
    }

    /// Espera a que alguien pida resincronizar y devuelve la época más alta
    /// del peer que no pudimos descifrar.
    pub async fn resync_requested(&self) -> u32 {
        loop {
            let pending = self.resync_epoch.swap(0, Ordering::Relaxed);
            if pending > 0 {
                return pending;
            }
            self.resync.notified().await;
        }
    }

    /// Inserta el secreto de una época (idempotente), evicta+zeroiza las más
    /// viejas si excede la ventana, y despierta a los `await_*`.
    pub fn insert(&self, epoch: u32, secret: [u8; 32]) {
        {
            let mut m = self.inner.lock();
            if m.contains_key(&epoch) {
                return;
            }
            m.insert(epoch, Zeroizing::new(secret));
            while m.len() > self.keep {
                let lo = *m.keys().next().expect("non-empty");
                m.remove(&lo); // Zeroizing::drop zeroiza el secreto evictado
            }
        }
        self.notify.notify_waiters();
    }

    /// Pisa el secreto de una época. Solo lo usa el respondedor cuando el
    /// iniciador repite esa época con una pubkey nueva (se reinició): ahí
    /// conservar el viejo dejaría a los dos extremos con secretos distintos
    /// para la misma época, que es indetectable —el enlace no lleva MAC— y
    /// corrompe en silencio todo lo que viaje por él.
    pub fn replace(&self, epoch: u32, secret: [u8; 32]) {
        {
            let mut m = self.inner.lock();
            m.insert(epoch, Zeroizing::new(secret)); // el viejo se zeroiza al drop
            while m.len() > self.keep {
                let lo = *m.keys().next().expect("non-empty");
                m.remove(&lo);
            }
        }
        self.notify.notify_waiters();
    }

    /// Descarta (y zeroiza) toda época anterior a `epoch`.
    ///
    /// La usa el re-enlace tras detectar que el peer se reinició: una vez
    /// negociado un bloque de épocas nuevo, las viejas ya no las tiene
    /// nadie al otro lado y conservarlas solo sirve para que `enc` siga
    /// eligiendo una que el peer no puede descifrar.
    pub fn prune_below(&self, epoch: u32) -> usize {
        let mut m = self.inner.lock();
        let keep = m.split_off(&epoch); // los < epoch se quedan en `m`
        let dropped = m.len();
        *m = keep; // los viejos se dropean aquí → Zeroizing los borra
        dropped
    }

    pub fn get(&self, epoch: u32) -> Option<Zeroizing<[u8; 32]>> {
        self.inner.lock().get(&epoch).cloned()
    }

    pub fn contains(&self, epoch: u32) -> bool {
        self.inner.lock().contains_key(&epoch)
    }

    /// Época establecida más alta, o `None` si aún no hay ninguna.
    pub fn highest(&self) -> Option<u32> {
        self.inner.lock().keys().next_back().copied()
    }

    /// Época viva más baja. Todo lo anterior fue evictado por la ventana
    /// deslizante (o nunca llegó, si el enlace se estableció con este
    /// extremo ya arrancado) y **no volverá jamás**: esperar por ello es
    /// tiempo tirado. Ver [`PqcKeySource::dec_keys`].
    pub fn lowest(&self) -> Option<u32> {
        self.inner.lock().keys().next().copied()
    }

    /// Espera (con deadline) a que exista el secreto de `epoch`.
    pub async fn await_epoch(&self, epoch: u32) -> Result<Zeroizing<[u8; 32]>> {
        let deadline = Instant::now() + SECRET_WAIT;
        loop {
            let notified = self.notify.notified(); // registrar ANTES de leer
            if let Some(s) = self.get(epoch) {
                return Ok(s);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_err());
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => return Err(timeout_err()),
            }
        }
    }

    /// Espera (con deadline) a que exista al menos una época; devuelve la más alta.
    pub async fn await_any(&self) -> Result<u32> {
        let deadline = Instant::now() + SECRET_WAIT;
        loop {
            let notified = self.notify.notified();
            if let Some(h) = self.highest() {
                return Ok(h);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(timeout_err());
            }
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(remaining) => return Err(timeout_err()),
            }
        }
    }
}

// ─────────────────────────── RekeyClock ─────────────────────────────────

/// Disparador de rotación de época, compartido entre el [`PqcKeySource`]
/// emisor (que cuenta claves) y el bucle de rotación del iniciador en
/// [`crate::pqc_handshake`] (que espera). Solo lo usa el lado iniciador.
pub struct RekeyClock {
    /// Rotar cada `n` claves producidas (0 = sin disparo por volumen).
    n: u64,
    counter: AtomicU64,
    notify: Notify,
}

impl RekeyClock {
    pub fn new(rekey_keys: u64) -> Arc<Self> {
        Arc::new(Self {
            n: rekey_keys,
            counter: AtomicU64::new(0),
            notify: Notify::new(),
        })
    }

    /// `true` si el disparo por volumen está desactivado (`n == 0`).
    pub fn is_volume_disabled(&self) -> bool {
        self.n == 0
    }

    /// Contabiliza `num` claves emitidas; dispara si se cruza un múltiplo de `n`.
    fn tick(&self, num: u32) {
        if self.n == 0 {
            return;
        }
        let before = self.counter.fetch_add(num as u64, Ordering::Relaxed);
        if before / self.n != (before + num as u64) / self.n {
            self.notify.notify_one();
        }
    }

    /// Espera a una condición de rotación: cruce de `n` claves O `t_secs`
    /// transcurridos (lo que ocurra primero). Si ambos disparadores están
    /// desactivados (`n==0 && t_secs==0`) no rota nunca (espera indefinida).
    pub async fn wait_rotate(&self, t_secs: u64) {
        match (self.n, t_secs) {
            (0, 0) => std::future::pending::<()>().await,
            (_, 0) => self.notify.notified().await,
            (0, _) => tokio::time::sleep(Duration::from_secs(t_secs)).await,
            (_, _) => {
                tokio::select! {
                    _ = self.notify.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(t_secs)) => {}
                }
            }
        }
    }
}

// ─────────────────────────── PqcKeySource ───────────────────────────────

/// [`KeySource`] de un enlace PQC con re-keying por épocas.
pub struct PqcKeySource {
    /// Longitud del material por clave (= `key_size_bits / 8`).
    key_bytes: usize,
    store: Arc<SecretStore>,
    /// Épocas pre-cargadas por delante de la activa. `enc` usa
    /// `highest − lookahead`; el iniciador mantiene `highest = activa + lookahead`.
    lookahead: u32,
    /// Reloj de rotación (Some solo en el iniciador; el respondedor sigue al
    /// iniciador vía `store.highest()`).
    clock: Option<Arc<RekeyClock>>,
}

impl PqcKeySource {
    pub fn new(
        key_size_bits: u32,
        store: Arc<SecretStore>,
        lookahead: u32,
        clock: Option<Arc<RekeyClock>>,
    ) -> Self {
        Self {
            key_bytes: (key_size_bits / 8) as usize,
            store,
            lookahead,
            clock,
        }
    }

    /// Época que usa `enc` ahora: `highest − lookahead` (≥ 0), **acotada por
    /// abajo a la época viva más baja**. Ambos extremos la calculan igual; el
    /// iniciador adelanta `highest` al rotar.
    ///
    /// La cota importa cuando este extremo acaba de reiniciar: su ventana
    /// empieza en la época que negoció al volver, no en cero. Sin acotar,
    /// `highest − lookahead` apunta a una época que nunca tuvo, `enc_keys`
    /// se bloquea 10 s y falla, y el enlace no encripta hasta que pasen
    /// `lookahead + 1` rotaciones —con el default `pqc_rekey_secs = 3600`
    /// y sin tráfico que fuerce rekey por volumen, eso son horas—.
    ///
    /// Subir a `lowest` es seguro: el peer conserva una ventana más ancha
    /// que la nuestra (él no se reinició), así que cualquier época que
    /// nosotros tengamos, él también.
    fn active_epoch(&self, highest: u32) -> u32 {
        let target = highest.saturating_sub(self.lookahead);
        match self.store.lowest() {
            Some(lo) if lo > target => lo,
            _ => target,
        }
    }
}

#[async_trait::async_trait]
impl KeySource for PqcKeySource {
    async fn enc_keys(&self, number: u32) -> Result<Vec<OtpKey>> {
        if number == 0 {
            return Ok(vec![]);
        }
        let highest = match self.store.highest() {
            Some(h) => h,
            None => self.store.await_any().await?,
        };
        let epoch = self.active_epoch(highest);
        let secret = self.store.await_epoch(epoch).await?;
        let mut out = Vec::with_capacity(number as usize);
        for _ in 0..number {
            let key_id = make_key_id(epoch);
            let material = derive_material(&secret, key_id.as_bytes(), self.key_bytes);
            out.push(OtpKey { key_id, material });
        }
        if let Some(c) = &self.clock {
            c.tick(number); // contabiliza para la rotación (solo iniciador)
        }
        Ok(out)
    }

    /// Deriva el material de cada `key_id` que **se pueda** derivar.
    ///
    /// Es best-effort a propósito. Antes bastaba un solo id irrecuperable
    /// para tumbar el lote entero: el `?` abortaba y el
    /// `keystore::dec_refill_loop` descartaba las 128 claves, incluidas
    /// las 127 buenas, tras haberse bloqueado 10 s esperando la que no
    /// existía. Con un lote envenenado por segundo el bucle pasaba el
    /// 100 % del tiempo bloqueado, el buffer DEC no subía de cero y TODOS
    /// los frames de ese enlace morían por timeout — el enlace quedaba
    /// muerto en un sentido mientras el contrario funcionaba.
    ///
    /// De dónde salen los ids irrecuperables: si un extremo se reinicia,
    /// el que sobrevive sigue teniendo claves ENC en buffer de épocas que
    /// el reiniciado nunca tendrá (arranca en la época que negocie al
    /// volver). Esas claves se emiten igual y llegan aquí.
    ///
    /// Las que no se puedan derivar simplemente no salen: su frame morirá
    /// —es indescifrable de verdad— pero sin arrastrar a los demás.
    async fn dec_keys(&self, ids: &[Uuid]) -> Result<Vec<OtpKey>> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let mut out = Vec::with_capacity(ids.len());
        // Épocas que ya han dado timeout en ESTE lote. Sin esto, N ids de
        // una época ausente costarían N × 10 s.
        let mut dead: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let (mut stale, mut timed_out) = (0usize, 0usize);
        let floor = self.store.lowest();
        for id in ids {
            let epoch = epoch_of(id.as_bytes());
            // Por debajo de la ventana viva: evictada o nunca recibida.
            // Descarte inmediato, sin esperar.
            if floor.is_some_and(|lo| epoch < lo) {
                stale += 1;
                continue;
            }
            if dead.contains(&epoch) {
                timed_out += 1;
                continue;
            }
            match self.store.await_epoch(epoch).await {
                Ok(secret) => out.push(OtpKey {
                    key_id: *id,
                    material: derive_material(&secret, id.as_bytes(), self.key_bytes),
                }),
                Err(_) => {
                    dead.insert(epoch);
                    timed_out += 1;
                }
            }
        }
        if stale > 0 || timed_out > 0 {
            tracing::warn!(
                requested = ids.len(),
                derived = out.len(),
                stale,
                timed_out,
                window = ?(self.store.lowest(), self.store.highest()),
                "qkc.pqc.dec_keys: claves indescifrables descartadas (el peer usó épocas \
                 que este extremo no tiene; suele ser que uno de los dos se reinició)",
            );
        }
        // Detectarlo y limitarse a contarlo dejaba el enlace tirando el 100 %
        // de lo que le llegaba para siempre (testbed 2026-08-03: un enlace con
        // dec_misses == dec_lookups que no se recuperaba solo). Se pide una
        // resincronización con la época MÁS ALTA que el peer haya usado: el
        // iniciador negociará un bloque por encima de ella y los dos extremos
        // vuelven a la misma ventana. `relink` ya está rate-limitado, así que
        // una tormenta de frames indescifrables no dispara una tormenta de
        // handshakes.
        if let Some(&peer_epoch) = dead.iter().max() {
            self.store.request_resync(peer_epoch);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::crypto::pqc::{kem_for, suite};

    fn fresh_secret(b: u8) -> [u8; 32] {
        [b; 32]
    }

    #[test]
    fn key_id_encodes_epoch() {
        for e in [0u32, 1, 7, 1000, u32::MAX] {
            let id = make_key_id(e);
            assert_eq!(epoch_of(id.as_bytes()), e);
        }
        // mismos 4 B de época, 12 B distintos (unicidad).
        assert_ne!(make_key_id(5), make_key_id(5));
    }

    #[test]
    fn derive_material_deterministic_and_sized() {
        let secret = [7u8; 32];
        let id = [3u8; 16];
        let a = derive_material(&secret, &id, 32);
        assert_eq!(a, derive_material(&secret, &id, 32));
        assert_eq!(a.len(), 32);
        assert_ne!(derive_material(&secret, &[4u8; 16], 32), a);
        assert_ne!(derive_material(&[8u8; 32], &id, 32), a);
        assert_eq!(derive_material(&secret, &id, 9000).len(), 9000);
    }

    #[test]
    fn store_evicts_and_zeroizes_old_epochs() {
        let store = SecretStore::new(2, 1000); // ventana acotada
        for e in 0..30u32 {
            store.insert(e, fresh_secret(e as u8));
        }
        assert_eq!(store.highest(), Some(29));
        // recientes presentes; las muy viejas evictadas (y zeroizadas al drop).
        assert!(store.get(29).is_some());
        assert!(store.get(28).is_some());
        assert!(store.get(0).is_none(), "oldest epoch evicted");
    }

    #[tokio::test]
    async fn await_epoch_waits_then_resolves() {
        let store = SecretStore::new(2, 1000);
        let s2 = Arc::clone(&store);
        let h = tokio::spawn(async move { s2.await_epoch(0).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        store.insert(0, fresh_secret(42));
        let got = h.await.unwrap().unwrap();
        assert_eq!(*got, fresh_secret(42));
    }

    /// Gate principal: tras un round-trip ML-KEM real por época, los dos
    /// extremos derivan material byte-idéntico para los mismos `key_id`, y
    /// distinto entre épocas.
    #[tokio::test]
    async fn both_ends_derive_identical_material_across_epochs() {
        let kem = kem_for(suite::ML_KEM_768).unwrap();
        // Dos épocas con secretos ML-KEM independientes.
        let store_a = SecretStore::new(2, 1000);
        let store_b = SecretStore::new(2, 1000);
        let mut secrets_epoch = Vec::new();
        for epoch in 0..2u32 {
            let kp = kem.keygen().unwrap();
            let encap = kem.encap(&kp.public).unwrap();
            let ss_peer = kem.decap(&kp.secret, &encap.ciphertext).unwrap();
            assert_eq!(encap.shared_secret, ss_peer);
            let mut a = [0u8; 32];
            a.copy_from_slice(&encap.shared_secret);
            store_a.insert(epoch, a);
            store_b.insert(epoch, a);
            secrets_epoch.push(a);
        }
        let src_a = PqcKeySource::new(256, Arc::clone(&store_a), 0, None);
        let src_b = PqcKeySource::new(256, Arc::clone(&store_b), 0, None);

        // enc en época 1 (highest=1, lookahead 0 → epoch 1).
        let enc = src_a.enc_keys(4).await.unwrap();
        for k in &enc {
            assert_eq!(epoch_of(k.key_id.as_bytes()), 1, "enc uses highest epoch");
        }
        let ids: Vec<Uuid> = enc.iter().map(|k| k.key_id).collect();
        let dec = src_b.dec_keys(&ids).await.unwrap();
        for (e, d) in enc.iter().zip(dec.iter()) {
            assert_eq!(e.key_id, d.key_id);
            assert_eq!(e.material, d.material, "enc/dec material identical");
        }
        // material de época 0 (mismo key_id-rand pero epoch 0) difiere del de época 1.
        let id0 = make_key_id(0);
        let id1 = {
            let mut b = *id0.as_bytes();
            b[0..4].copy_from_slice(&1u32.to_be_bytes());
            Uuid::from_bytes(b)
        };
        let m0 = src_b.dec_keys(&[id0]).await.unwrap()[0].material.clone();
        let m1 = src_b.dec_keys(&[id1]).await.unwrap()[0].material.clone();
        assert_ne!(m0, m1, "distinta época ⇒ distinto material");
    }

    /// Un id de una época que este extremo nunca tuvo (el peer se reinició
    /// y nosotros no) NO puede llevarse por delante al resto del lote.
    ///
    /// Es el fallo que dejaba un enlace PQC muerto en un solo sentido: el
    /// `?` abortaba las 128 claves del lote y, de paso, bloqueaba el
    /// `dec_refill_loop` 10 s esperando un secreto que no iba a llegar.
    #[tokio::test]
    async fn one_unrecoverable_id_does_not_kill_the_batch() {
        let store = SecretStore::new(0, 1000);
        // Ventana viva = épocas 5 y 6. La 1 se perdió.
        store.insert(5, fresh_secret(5));
        store.insert(6, fresh_secret(6));
        let src = PqcKeySource::new(256, Arc::clone(&store), 0, None);

        let stale = make_key_id(1);
        let good: Vec<Uuid> = (0..4).map(|_| make_key_id(6)).collect();
        let mut ids = vec![stale];
        ids.extend(good.iter().copied());

        let started = std::time::Instant::now();
        let out = src.dec_keys(&ids).await.unwrap();

        assert_eq!(out.len(), 4, "las 4 derivables sobreviven al id envenenado");
        assert!(
            out.iter().all(|k| k.key_id != stale),
            "la irrecuperable no se inventa",
        );
        assert_eq!(
            out.iter().map(|k| k.key_id).collect::<Vec<_>>(),
            good,
            "orden y correspondencia intactos",
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "descarte inmediato: no se espera por una época bajo la ventana",
        );
    }

    /// Un extremo recién reiniciado sólo tiene las épocas negociadas desde
    /// que volvió. `enc` debe elegir una que TENGA, no `highest − lookahead`
    /// a ciegas: si no, no encripta hasta pasadas `lookahead + 1` rotaciones.
    #[tokio::test]
    async fn enc_after_a_restart_uses_an_epoch_this_end_actually_has() {
        let store = SecretStore::new(2, 1000);
        // Ventana tras reiniciar: sólo 8 y 9. La 7 (= 9 − lookahead) falta.
        store.insert(8, fresh_secret(8));
        store.insert(9, fresh_secret(9));
        let src = PqcKeySource::new(256, Arc::clone(&store), 2, None);

        let started = std::time::Instant::now();
        let keys = src.enc_keys(3).await.unwrap();

        assert_eq!(keys.len(), 3);
        for k in &keys {
            assert_eq!(
                epoch_of(k.key_id.as_bytes()),
                8,
                "usa la más baja viva, no la 7 que no tiene",
            );
        }
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "sin bloqueo esperando una época inexistente",
        );
    }

    /// Tras un re-enlace, el iniciador negocia un bloque de épocas nuevo y
    /// poda las viejas. El invariante que hace que el enlace vuelva a
    /// funcionar es que `enc` acabe eligiendo `base`, que es justo lo único
    /// que el respondedor recién reiniciado tiene.
    #[tokio::test]
    async fn after_a_relink_both_ends_land_on_the_same_epoch() {
        const LOOKAHEAD: u32 = 2;
        let base = 3u32;

        // Iniciador: venía con 0..2 y negocia 3..5 encima.
        let ini = SecretStore::new(LOOKAHEAD, 1000);
        for e in 0..=2u32 {
            ini.insert(e, fresh_secret(e as u8));
        }
        for e in base..=base + LOOKAHEAD {
            ini.insert(e, fresh_secret(e as u8));
        }
        // Respondedor reiniciado: sólo tiene lo negociado tras volver.
        let resp = SecretStore::new(LOOKAHEAD, 1000);
        for e in base..=base + LOOKAHEAD {
            resp.insert(e, fresh_secret(e as u8));
        }

        assert_eq!(ini.prune_below(base), 3, "se tiran las tres viejas");
        assert_eq!(ini.lowest(), Some(base));
        assert!(ini.get(2).is_none(), "la época podada ya no está");

        let src_ini = PqcKeySource::new(256, Arc::clone(&ini), LOOKAHEAD, None);
        let src_resp = PqcKeySource::new(256, Arc::clone(&resp), LOOKAHEAD, None);
        let k_ini = src_ini.enc_keys(1).await.unwrap();
        let k_resp = src_resp.enc_keys(1).await.unwrap();

        assert_eq!(epoch_of(k_ini[0].key_id.as_bytes()), base);
        assert_eq!(epoch_of(k_resp[0].key_id.as_bytes()), base);
        // Y lo que emite cada uno es descifrable por el otro.
        assert!(src_resp.dec_keys(&[k_ini[0].key_id]).await.unwrap().len() == 1);
        assert!(src_ini.dec_keys(&[k_resp[0].key_id]).await.unwrap().len() == 1);
    }

    /// Con la ventana completa, la cota no cambia nada.
    #[tokio::test]
    async fn enc_still_honours_lookahead_when_the_window_is_complete() {
        let store = SecretStore::new(2, 1000);
        for e in 0..=9u32 {
            store.insert(e, fresh_secret(e as u8));
        }
        let src = PqcKeySource::new(256, Arc::clone(&store), 2, None);
        let keys = src.enc_keys(1).await.unwrap();
        assert_eq!(
            epoch_of(keys[0].key_id.as_bytes()),
            7,
            "highest − lookahead"
        );
    }

    /// Varios ids de una misma época ausente cuestan UNA espera, no N.
    #[tokio::test]
    async fn a_missing_epoch_is_waited_for_once_per_batch() {
        let store = SecretStore::new(0, 1000);
        store.insert(5, fresh_secret(5));
        let src = PqcKeySource::new(256, Arc::clone(&store), 0, None);
        // Época 9 > la ventana: es futura, así que sí se espera... pero
        // sólo la primera vez. Con SECRET_WAIT=10 s, tres esperas serían
        // 30 s; una sola son 10 s.
        let ids: Vec<Uuid> = (0..3).map(|_| make_key_id(9)).collect();
        let started = std::time::Instant::now();
        let out = src.dec_keys(&ids).await.unwrap();
        assert!(out.is_empty());
        assert!(
            started.elapsed() < SECRET_WAIT * 2,
            "se esperó una vez por época, no una por clave",
        );
    }

    /// El lado DEC no puede limitarse a contar los frames indescifrables: si
    /// las ventanas de los dos extremos se separan (un reinicio deja al que
    /// arranca abajo y al otro arriba), el enlace tira el 100 % de lo que
    /// recibe y no se recupera solo. Pedir resincronización con la época MÁS
    /// ALTA vista es lo que permite al iniciador negociar por encima de las
    /// dos ventanas.
    #[tokio::test]
    async fn an_unknown_peer_epoch_asks_for_a_resync_with_that_epoch() {
        let store = SecretStore::new(0, 1000);
        store.insert(5, fresh_secret(5));
        let src = PqcKeySource::new(256, Arc::clone(&store), 0, None);

        // El peer cifra con 9 y con 11: ninguna existe aquí.
        let ids = vec![make_key_id(9), make_key_id(11)];
        let out = src.dec_keys(&ids).await.unwrap();
        assert!(out.is_empty(), "no se puede derivar ninguna");

        let asked = tokio::time::timeout(Duration::from_secs(1), store.resync_requested())
            .await
            .expect("debe haber una petición pendiente");
        assert_eq!(asked, 11, "se pide con la época más alta, no la primera");
    }

    /// Sin frames indescifrables no se pide nada: el bucle de rotación no
    /// debe despertarse ni renegociar porque sí.
    #[tokio::test]
    async fn a_decryptable_batch_asks_for_nothing() {
        let store = SecretStore::new(0, 1000);
        store.insert(5, fresh_secret(5));
        let src = PqcKeySource::new(256, Arc::clone(&store), 0, None);

        let out = src.dec_keys(&[make_key_id(5)]).await.unwrap();
        assert_eq!(out.len(), 1);

        assert!(
            tokio::time::timeout(Duration::from_millis(200), store.resync_requested())
                .await
                .is_err(),
            "no debería haber petición de resincronización",
        );
    }
}
