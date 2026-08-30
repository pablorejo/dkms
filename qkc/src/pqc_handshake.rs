//! Handshake ML-KEM de un enlace PQC, sobre el canal TCP QKC↩QKC, con
//! **re-keying por épocas**.
//!
//! Cada "época" tiene su propio secreto de 32 B acordado por un ML-KEM
//! independiente. El [`crate::pqc_source::SecretStore`] guarda los secretos
//! por época y [`crate::pqc_source::PqcKeySource`] deriva de ellos. Las épocas
//! futuras se **pre-cargan** (`lookahead`) para que la rotación sea
//! transparente; las viejas se zeroizan al evictar (forward secrecy).
//!
//! Roles deterministas por id: el QKC con `qkc_id` **menor** es el iniciador
//! (genera keypair y dirige las rotaciones), el mayor el respondedor.
//!
//! ```text
//! Iniciador (id menor)                      Respondedor (id mayor)
//!   keygen(época) → (pk, sk)
//!   ── FRAME_PQC_KEM_INIT{época‖pk} ─────►
//!                                            encap(pk) → (ct, ss)
//!                                            store.insert(época, ss); cachea ct
//!   ◄──── FRAME_PQC_KEM_RESP{época‖ct} ──
//!   decap(sk, ct) → ss; store.insert(época, ss)
//! ```
//!
//! El payload de INIT/RESP lleva un prefijo de **4 B de época** (big-endian)
//! antes de la pubkey/ciphertext. No hay frames nuevos.
//!
//! Robustez: el iniciador **reenvía el mismo INIT** por época (misma pk/sk) con
//! backoff hasta que su secreto está. El respondedor **cachea `(pubkey, ct)` por
//! época** y reenvía el ct ante INITs duplicados (re-encapsular daría otro
//! secreto). La pubkey forma parte de la caché a propósito: si el INIT llega con
//! **otra** pubkey, el iniciador se reinició y hay que re-encapsular y pisar el
//! secreto. Reenviarle el ct viejo no daría error —ML-KEM aplica *implicit
//! rejection* y devuelve un secreto pseudoaleatorio— sino dos extremos con
//! secretos distintos para la misma época, sin MAC que lo delate y corrompiendo
//! en silencio hasta las claves que reciben los SAEs.

use std::collections::HashMap;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use common::crypto::link_mac::{self, TAG_INIT, TAG_LEN, TAG_RESP};
use common::crypto::pqc_sign::{self, SIGNATURE_LEN};
use parking_lot::Mutex;
use tracing::{debug, info, warn};
use wire::{
    Frame, FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_INIT_AUTH, FRAME_PQC_KEM_INIT_SIGNED,
    FRAME_PQC_KEM_RESP, FRAME_PQC_KEM_RESP_AUTH, FRAME_PQC_KEM_RESP_SIGNED,
};
use zeroize::Zeroizing;

use crate::{
    config::PqcAuth,
    pqc_source::{RekeyClock, SecretStore},
    transport::peer_client::PeerOut,
};

/// Reintento del INIT mientras una época no completa.
const INIT_RETRY: Duration = Duration::from_millis(300);

/// Mínimo entre re-enlaces. Si el peer flapea, no queremos una tanda de
/// ML-KEM por cada rebote del socket.
const RELINK_MIN_INTERVAL: Duration = Duration::from_secs(5);

/// Coordinador del handshake ML-KEM (multi-época) de UN enlace PQC.
pub struct PqcHandshake {
    suite: String,
    my_id: u32,
    peer_id: u32,
    peer_addr: String,
    peer_out: Arc<PeerOut>,
    store: Arc<SecretStore>,
    /// Disparador de rotación (lo alimenta el `PqcKeySource` emisor).
    clock: Arc<RekeyClock>,
    /// Épocas pre-cargadas por delante de la activa.
    lookahead: u32,
    /// Tope de edad de una época en segundos (0 = sin disparo por tiempo).
    rekey_secs: u64,
    /// Decap keys del iniciador por época, vivas hasta que llega el RESP.
    pending_sk: Mutex<HashMap<u32, Zeroizing<Vec<u8>>>>,
    /// Por época, lo que el respondedor encapsuló: `(pubkey del iniciador,
    /// ciphertext)`. La pubkey va en la clave porque el ciphertext SOLO sirve
    /// para esa pubkey — ver [`PqcHandshake::handle_init`].
    resp_cache: Mutex<HashMap<u32, (Vec<u8>, Vec<u8>)>>,
    /// Último re-enlace, para el rate-limit de [`RELINK_MIN_INTERVAL`].
    last_relink: Mutex<Option<Instant>>,
    /// PSK del enlace para autenticar el handshake por HMAC (§Fase 5).
    /// `None` → sin PSK.
    psk: Option<Vec<u8>>,
    /// Semilla ML-DSA de firma de ESTE nodo (modo `sign`). `None` → no firma.
    sign_seed: Option<Vec<u8>>,
    /// Clave pública ML-DSA del PEER para verificar sus firmas (modo `sign`).
    peer_verify_key: Option<Vec<u8>>,
    /// Política de autenticación (off/prefer/require/sign).
    auth: PqcAuth,
    /// Tamaño de clave en bits, atado en el MAC/firma (cierra el mismatch).
    key_size_bits: u32,
}

/// Cómo llegó autenticado un frame de handshake, según su tipo.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RecvAuth {
    /// 0x21/0x22 — en claro.
    Plain,
    /// 0x23/0x24 — HMAC-PSK.
    Hmac,
    /// 0x26/0x27 — firma ML-DSA.
    Signed,
}

/// Parte un payload `época_be(4) ‖ blob` en `(época, blob)`.
fn split_epoch(payload: &[u8]) -> Option<(u32, &[u8])> {
    if payload.len() < 4 {
        return None;
    }
    let epoch = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Some((epoch, &payload[4..]))
}

impl PqcHandshake {
    #[allow(clippy::too_many_arguments)] // constructor de un enlace; agrupar no aporta
    pub fn new(
        suite: String,
        my_id: u32,
        peer_id: u32,
        peer_addr: String,
        peer_out: Arc<PeerOut>,
        store: Arc<SecretStore>,
        clock: Arc<RekeyClock>,
        lookahead: u32,
        rekey_secs: u64,
        key_size_bits: u32,
        psk: Option<Vec<u8>>,
        auth: PqcAuth,
        sign_seed: Option<Vec<u8>>,
        peer_verify_key: Option<Vec<u8>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            suite,
            my_id,
            peer_id,
            peer_addr,
            peer_out,
            store,
            clock,
            lookahead,
            rekey_secs,
            pending_sk: Mutex::new(HashMap::new()),
            resp_cache: Mutex::new(HashMap::new()),
            last_relink: Mutex::new(None),
            psk,
            sign_seed,
            peer_verify_key,
            auth,
            key_size_bits,
        })
    }

    /// ¿Enviamos/exigimos HMAC-PSK? Requiere PSK y modo prefer/require.
    fn hmac_active(&self) -> bool {
        self.psk.is_some() && matches!(self.auth, PqcAuth::Prefer | PqcAuth::Require)
    }

    fn is_initiator(&self) -> bool {
        self.my_id < self.peer_id
    }

    fn publish(&self, epoch: u32, ss: Vec<u8>) {
        self.publish_inner(epoch, ss, false)
    }

    /// Como [`publish`](Self::publish) pero pisando el secreto que hubiera.
    /// Solo para el re-handshake de una época que el peer repite con otra
    /// pubkey: quedarnos con el viejo dejaría los dos extremos en desacuerdo.
    fn publish_replacing(&self, epoch: u32, ss: Vec<u8>) {
        self.publish_inner(epoch, ss, true)
    }

    fn publish_inner(&self, epoch: u32, ss: Vec<u8>, replace: bool) {
        if ss.len() != 32 {
            warn!(
                peer = self.peer_id,
                len = ss.len(),
                "qkc.pqc: secret not 32 B"
            );
            return;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&ss);
        if replace {
            self.store.replace(epoch, arr);
        } else {
            self.store.insert(epoch, arr);
        }
        info!(
            me = self.my_id,
            peer = self.peer_id,
            epoch,
            replaced = replace,
            "qkc.pqc.handshake.established"
        );
    }

    /// Envía un mensaje del handshake. `is_init` elige INIT vs RESP. El modo
    /// `pqc_auth` decide el frame:
    /// - `sign` + seed → 0x26/0x27 con `payload = época‖blob‖firma_MLDSA`.
    /// - `prefer`/`require` + psk → 0x23/0x24 con `payload = época‖blob‖tag_HMAC`.
    /// - resto → 0x21/0x22 en claro.
    fn send_handshake(&self, is_init: bool, epoch: u32, blob: &[u8]) {
        let (plain, hmac_kind, signed_kind, tag_kind) = if is_init {
            (
                FRAME_PQC_KEM_INIT,
                FRAME_PQC_KEM_INIT_AUTH,
                FRAME_PQC_KEM_INIT_SIGNED,
                TAG_INIT,
            )
        } else {
            (
                FRAME_PQC_KEM_RESP,
                FRAME_PQC_KEM_RESP_AUTH,
                FRAME_PQC_KEM_RESP_SIGNED,
                TAG_RESP,
            )
        };
        let mut payload = Vec::with_capacity(4 + blob.len() + SIGNATURE_LEN);
        payload.extend_from_slice(&epoch.to_be_bytes());
        payload.extend_from_slice(blob);

        let kind = if self.auth == PqcAuth::Sign {
            match self.sign_seed.as_deref() {
                Some(seed) => match pqc_sign::sign_handshake(
                    seed,
                    tag_kind,
                    epoch,
                    self.my_id,
                    self.peer_id,
                    blob,
                    &self.suite,
                    self.key_size_bits,
                ) {
                    Ok(sig) => {
                        payload.extend_from_slice(&sig);
                        signed_kind
                    }
                    Err(e) => {
                        warn!(peer = self.peer_id, error = ?e, "qkc.pqc: fallo al firmar; envío en claro");
                        plain
                    }
                },
                None => {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: modo sign sin sign_secret_seed; envío en claro"
                    );
                    plain
                }
            }
        } else if let Some(psk) = self.psk.as_deref().filter(|_| self.hmac_active()) {
            let mac = link_mac::tag(
                psk,
                tag_kind,
                epoch,
                self.my_id,
                self.peer_id,
                blob,
                &self.suite,
                self.key_size_bits,
            );
            payload.extend_from_slice(&mac);
            hmac_kind
        } else {
            plain
        };
        let mut f = Frame::empty(kind);
        f.sender_id = self.my_id;
        f.receiver_id = self.peer_id;
        f.dest_final = self.peer_id;
        f.payload = payload;
        self.peer_out.send(self.peer_id, &self.peer_addr, f);
    }

    /// Extrae `(época, blob)` de un payload de handshake aplicando la política.
    /// `recv` indica cómo llegó el frame (claro / HMAC / firmado). Devuelve
    /// `None` (descartar) si la autenticación no valida o si la política exige
    /// autenticación y el frame llegó sin ella. En recepción el MAC/firma se
    /// computó con (sender=peer, receiver=yo). `tag_kind` es TAG_INIT/TAG_RESP.
    fn accept<'a>(
        &self,
        tag_kind: &[u8],
        payload: &'a [u8],
        recv: RecvAuth,
    ) -> Option<(u32, &'a [u8])> {
        match recv {
            RecvAuth::Signed => {
                let Some(vk) = self.peer_verify_key.as_deref() else {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: frame firmado pero sin peer_verify_key; descarto"
                    );
                    return None;
                };
                if payload.len() < 4 + SIGNATURE_LEN {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: payload firmado demasiado corto"
                    );
                    return None;
                }
                let (msg, sig) = payload.split_at(payload.len() - SIGNATURE_LEN);
                let (epoch, blob) = split_epoch(msg)?;
                if pqc_sign::verify_handshake(
                    vk,
                    tag_kind,
                    epoch,
                    self.peer_id,
                    self.my_id,
                    blob,
                    &self.suite,
                    self.key_size_bits,
                    sig,
                )
                .is_err()
                {
                    warn!(
                        peer = self.peer_id,
                        epoch, "qkc.pqc: firma ML-DSA inválida; descarto"
                    );
                    return None;
                }
                Some((epoch, blob))
            }
            RecvAuth::Hmac => {
                let Some(psk) = self.psk.as_deref() else {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: frame HMAC pero sin link_psk; descarto"
                    );
                    return None;
                };
                if payload.len() < 4 + TAG_LEN {
                    warn!(peer = self.peer_id, "qkc.pqc: payload HMAC demasiado corto");
                    return None;
                }
                let (msg, mac) = payload.split_at(payload.len() - TAG_LEN);
                let (epoch, blob) = split_epoch(msg)?;
                if link_mac::verify(
                    psk,
                    tag_kind,
                    epoch,
                    self.peer_id,
                    self.my_id,
                    blob,
                    &self.suite,
                    self.key_size_bits,
                    mac,
                )
                .is_err()
                {
                    warn!(
                        peer = self.peer_id,
                        epoch, "qkc.pqc: MAC de handshake inválido; descarto"
                    );
                    return None;
                }
                Some((epoch, blob))
            }
            RecvAuth::Plain => {
                // La política exige autenticación → descartar el frame en claro.
                let require_auth = self.auth == PqcAuth::Sign
                    || (self.auth == PqcAuth::Require && self.psk.is_some());
                if require_auth {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: la política exige handshake autenticado, descarto el frame en claro"
                    );
                    return None;
                }
                split_epoch(payload)
            }
        }
    }

    /// Arranca la tarea de rotación/pre-carga. No-op en el respondedor (que es
    /// reactivo: encapsula al recibir cada INIT). El iniciador establece las
    /// épocas `0..=lookahead` y luego añade una por cada disparo de rotación
    /// **o** cuando el peer se reconecta (ver [`Self::relink`]).
    pub fn spawn_rotation(self: &Arc<Self>) {
        if !self.is_initiator() {
            // El respondedor no puede renegociar —solo el lex-menor manda
            // INIT—, pero su lado DEC sí detecta cuando las ventanas se han
            // separado. Sin esto la petición de resincronización se quedaría
            // en un atómico que nadie lee y el operador no vería más que
            // frames descartados. Si el enlace falla en los dos sentidos, el
            // iniciador lo detectará por su cuenta y lo arreglará; si solo
            // falla en este, hace falta intervención.
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let peer_epoch = me.store.resync_requested().await;
                    warn!(
                        me = me.my_id,
                        peer = me.peer_id,
                        peer_epoch,
                        mia = ?(me.store.lowest(), me.store.highest()),
                        "qkc.pqc: el peer cifra con épocas que no tengo y este extremo es el \
                         respondedor, así que no puede renegociar. Si el iniciador no lo \
                         detecta también por su lado, el enlace no se recupera solo",
                    );
                }
            });
            return;
        }
        // Pedimos la señal ANTES del primer INIT: así el slot existe y no
        // se nos escapa una reconexión temprana.
        let reconnect = self
            .peer_out
            .reconnect_signal(self.peer_id, &self.peer_addr);
        let me = self.clone();
        tokio::spawn(async move {
            for epoch in 0..=me.lookahead {
                me.establish(epoch).await;
            }
            loop {
                // Un único dueño de `establish`: este bucle. El re-enlace
                // NO puede ir en su propia task o dos `establish` de la
                // misma época pisarían `pending_sk` y el RESP se
                // decapsularía con la sk equivocada.
                if me.clock_disabled() {
                    // Sin rotación configurada seguimos atendiendo
                    // reconexiones y peticiones de resincronización: son lo
                    // que resucita un enlace cuyo respondedor se reinició.
                    tokio::select! {
                        _ = reconnect.notified() => me.relink(0).await,
                        peer_epoch = me.store.resync_requested() => me.relink(peer_epoch).await,
                    }
                    continue;
                }
                tokio::select! {
                    _ = me.clock.wait_rotate(me.rekey_secs) => {
                        let next = me.store.highest().unwrap_or(0).saturating_add(1);
                        me.establish(next).await;
                    }
                    _ = reconnect.notified() => me.relink(0).await,
                    // El lado DEC ha visto al peer cifrar con épocas que no
                    // tenemos. Es la única forma de enterarse: las ventanas de
                    // los dos extremos pueden separarse tras un reinicio y
                    // nadie lo nota hasta que llega un frame indescifrable.
                    peer_epoch = me.store.resync_requested() => me.relink(peer_epoch).await,
                }
            }
        });
    }

    /// Renegocia el enlace entero tras una reconexión con el peer.
    ///
    /// El respondedor no puede iniciar nada —si mandara INIT él, nosotros
    /// conservaríamos nuestro secreto viejo y él adoptaría el nuevo, que es
    /// justo la divergencia silenciosa que hay que evitar—, así que si se
    /// reinicia se queda **sin ninguna época** y el enlace muere en los dos
    /// sentidos: `establish` sale antes de tiempo para las épocas que
    /// nosotros ya tenemos, así que nunca le reenviamos el INIT. Con el
    /// default `pqc_rekey_secs = 3600` y un enlace ocioso eso era una hora
    /// de enlace levantado y vacío.
    ///
    /// Que el socket se caiga y vuelva es la señal: solo pasa si el peer se
    /// fue. Negociamos un bloque de épocas NUEVO por encima de las actuales
    /// (nunca reutilizamos números: el mismo número con otro secreto es
    /// indetectable) y podamos las viejas, que ya no tiene nadie enfrente.
    /// Una reconexión por un corte de red sin reinicio también dispara
    /// esto; cuesta tres ML-KEM y no rompe nada.
    ///
    /// `peer_epoch` es la época más alta que el peer ha usado y que nosotros
    /// no tenemos (0 si no se sabe). El bloque nuevo se negocia **por encima
    /// de las dos ventanas**: si nos quedáramos en la nuestra, el peer —cuya
    /// ventana está más alta— seguiría cifrando con las suyas y el enlace
    /// no convergería nunca. Es exactamente lo que se vio en el testbed el
    /// 2026-08-03: `dec_misses == dec_lookups` de forma permanente.
    async fn relink(&self, peer_epoch: u32) {
        let now = Instant::now();
        {
            let mut last = self.last_relink.lock();
            if let Some(prev) = *last {
                if now.duration_since(prev) < RELINK_MIN_INTERVAL {
                    debug!(
                        peer = self.peer_id,
                        "qkc.pqc.relink omitido (demasiado seguido)"
                    );
                    return;
                }
            }
            *last = Some(now);
        }
        let mine = self.store.highest().map(|h| h + 1).unwrap_or(0);
        let base = mine.max(peer_epoch.saturating_add(1));
        warn!(
            me = self.my_id,
            peer = self.peer_id,
            base,
            peer_epoch,
            "qkc.pqc.relink: renegocio el enlace (reconexión del peer o épocas suyas que no tengo)",
        );
        for epoch in base..=base.saturating_add(self.lookahead) {
            self.establish(epoch).await;
        }
        // Podamos DESPUÉS de negociar: si vaciáramos antes, `enc_keys` se
        // quedaría sin ninguna época mientras dura el handshake.
        let dropped = self.store.prune_below(base);
        self.pending_sk.lock().retain(|e, _| *e >= base);
        self.resp_cache.lock().retain(|e, _| *e >= base);
        info!(
            me = self.my_id,
            peer = self.peer_id,
            base,
            epocas_descartadas = dropped,
            "qkc.pqc.relink completado",
        );
    }

    fn clock_disabled(&self) -> bool {
        // n==0 (sin volumen) y rekey_secs==0 (sin tiempo) ⇒ no rota.
        self.rekey_secs == 0 && self.clock.is_volume_disabled()
    }

    /// Iniciador: establece el secreto de `epoch` (idempotente). Genera el
    /// keypair, reenvía INIT{epoch,pk} con backoff hasta que el RESP llega y
    /// `store` tiene la época.
    async fn establish(&self, epoch: u32) {
        if self.store.contains(epoch) {
            return;
        }
        let kem = match common::crypto::pqc::kem_for(&self.suite) {
            Ok(k) => k,
            Err(e) => {
                warn!(peer = self.peer_id, error = %e, "qkc.pqc: kem_for failed");
                return;
            }
        };
        let kp = match kem.keygen() {
            Ok(k) => k,
            Err(e) => {
                warn!(peer = self.peer_id, error = %e, "qkc.pqc: keygen failed");
                return;
            }
        };
        self.pending_sk
            .lock()
            .insert(epoch, Zeroizing::new(kp.secret));
        let pubkey = kp.public;
        let mut attempts: u64 = 0;
        while !self.store.contains(epoch) {
            self.send_handshake(true, epoch, &pubkey);
            attempts += 1;
            if attempts.is_multiple_of(20) {
                debug!(
                    peer = self.peer_id,
                    epoch, attempts, "qkc.pqc.init_retrying"
                );
            }
            tokio::time::sleep(INIT_RETRY).await;
        }
        self.pending_sk.lock().remove(&epoch);
    }

    /// Respondedor: llegó un INIT `época‖pubkey` (`authed` = frame 0x23).
    pub fn handle_init(&self, payload: &[u8], recv: RecvAuth) {
        let Some((epoch, peer_pubkey)) = self.accept(TAG_INIT, payload, recv) else {
            return;
        };
        // Idempotencia por época: si ya encapsulamos PARA ESTA MISMA pubkey,
        // reenvía el ct cacheado (es un INIT duplicado).
        //
        // La comparación de la pubkey no es un detalle: si el iniciador se
        // reinició, vuelve con un keypair nuevo y repite las épocas 0..N.
        // Devolverle el ciphertext viejo era catastrófico y silencioso —
        // ML-KEM no falla al decapsular un ciphertext ajeno, aplica *implicit
        // rejection* y entrega un secreto pseudoaleatorio. Los dos extremos
        // quedaban con secretos DISTINTOS para la misma época, ambos
        // convencidos de haber cerrado el handshake. Como el OTP del enlace
        // no lleva MAC, los frames se "descifraban" a basura sin un solo
        // error, y la basura subía hasta el DKMS: los dos SAEs de una
        // petición ETSI-014 acababan con claves distintas y nadie se
        // enteraba. Con pubkey nueva, handshake nuevo.
        let cached = self.resp_cache.lock().get(&epoch).cloned();
        let mut stale = false;
        match cached {
            Some((pk, ct)) if pk == peer_pubkey => {
                self.send_handshake(false, epoch, &ct);
                return;
            }
            Some(_) => {
                warn!(
                    peer = self.peer_id,
                    epoch,
                    "qkc.pqc: el peer repite la época con otra pubkey (se reinició); \
                     re-encapsulo y reemplazo el secreto",
                );
                stale = true;
            }
            None => {}
        }
        let kem = match common::crypto::pqc::kem_for(&self.suite) {
            Ok(k) => k,
            Err(e) => {
                warn!(peer = self.peer_id, error = %e, "qkc.pqc: kem_for failed");
                return;
            }
        };
        let encap = match kem.encap(peer_pubkey) {
            Ok(e) => e,
            Err(e) => {
                warn!(peer = self.peer_id, error = %e, "qkc.pqc: encap failed");
                return;
            }
        };
        if stale {
            self.publish_replacing(epoch, encap.shared_secret);
        } else {
            self.publish(epoch, encap.shared_secret);
        }
        self.resp_cache
            .lock()
            .insert(epoch, (peer_pubkey.to_vec(), encap.ciphertext.clone()));
        self.send_handshake(false, epoch, &encap.ciphertext);
    }

    /// Iniciador: llegó el RESP `época‖ciphertext` (`authed` = frame 0x24).
    pub fn handle_resp(&self, payload: &[u8], recv: RecvAuth) {
        let Some((epoch, ciphertext)) = self.accept(TAG_RESP, payload, recv) else {
            return;
        };
        if self.store.contains(epoch) {
            return;
        }
        let sk = match self.pending_sk.lock().get(&epoch).cloned() {
            Some(sk) => sk,
            None => {
                warn!(
                    peer = self.peer_id,
                    epoch, "qkc.pqc: RESP without pending sk"
                );
                return;
            }
        };
        let kem = match common::crypto::pqc::kem_for(&self.suite) {
            Ok(k) => k,
            Err(e) => {
                warn!(peer = self.peer_id, error = %e, "qkc.pqc: kem_for failed");
                return;
            }
        };
        match kem.decap(&sk[..], ciphertext) {
            Ok(ss) => self.publish(epoch, ss),
            Err(e) => warn!(peer = self.peer_id, error = %e, "qkc.pqc: decap failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pqc_source::epoch_of;

    fn handshake(my_id: u32, peer_id: u32) -> Arc<PqcHandshake> {
        handshake_auth(my_id, peer_id, None, PqcAuth::Off)
    }

    fn handshake_auth(
        my_id: u32,
        peer_id: u32,
        psk: Option<Vec<u8>>,
        auth: PqcAuth,
    ) -> Arc<PqcHandshake> {
        handshake_full(my_id, peer_id, psk, auth, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn handshake_full(
        my_id: u32,
        peer_id: u32,
        psk: Option<Vec<u8>>,
        auth: PqcAuth,
        sign_seed: Option<Vec<u8>>,
        peer_verify_key: Option<Vec<u8>>,
    ) -> Arc<PqcHandshake> {
        PqcHandshake::new(
            common::crypto::pqc::suite::ML_KEM_768.to_string(),
            my_id,
            peer_id,
            "127.0.0.1:1".to_string(),
            Arc::new(PeerOut::new()),
            SecretStore::new(2, 1000),
            RekeyClock::new(1000),
            2,
            3600,
            1024,
            psk,
            auth,
            sign_seed,
            peer_verify_key,
        )
    }

    /// Round-trip in-process por época: el respondedor encapsula sobre la
    /// pubkey del iniciador y el iniciador decapsula; ambos `store` quedan con
    /// el MISMO secreto para esa época. (Invocamos los handlers con los blobs
    /// `época‖payload` que viajarían en el frame.)
    #[tokio::test]
    async fn handshake_round_trip_per_epoch() {
        for epoch in [0u32, 1, 7] {
            let ini = handshake(1, 2);
            let resp = handshake(2, 1);
            let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
            let kp = kem.keygen().unwrap();
            ini.pending_sk
                .lock()
                .insert(epoch, Zeroizing::new(kp.secret.clone()));

            // INIT = época ‖ pubkey
            let mut init_payload = epoch.to_be_bytes().to_vec();
            init_payload.extend_from_slice(&kp.public);
            resp.handle_init(&init_payload, RecvAuth::Plain);
            let ct = resp.resp_cache.lock().get(&epoch).cloned().unwrap().1;

            // RESP = época ‖ ciphertext
            let mut resp_payload = epoch.to_be_bytes().to_vec();
            resp_payload.extend_from_slice(&ct);
            ini.handle_resp(&resp_payload, RecvAuth::Plain);

            let s_ini = ini.store.get(epoch).unwrap();
            let s_resp = resp.store.get(epoch).unwrap();
            assert_eq!(
                *s_ini, *s_resp,
                "both ends share the same secret for the epoch"
            );
        }
    }

    const PSK: &[u8] = b"link-psk-shared-between-the-two-qkcs";

    // Construye un payload INIT autenticado tal como lo emitiría el iniciador
    // (my_id, peer_id) → tag con (my_id, peer_id).
    fn authed(kind: &[u8], epoch: u32, sender: u32, receiver: u32, blob: &[u8]) -> Vec<u8> {
        let mut p = epoch.to_be_bytes().to_vec();
        p.extend_from_slice(blob);
        let mac = link_mac::tag(PSK, kind, epoch, sender, receiver, blob, "ml-kem-768", 1024);
        p.extend_from_slice(&mac);
        p
    }

    #[tokio::test]
    async fn authenticated_handshake_round_trip() {
        let ini = handshake_auth(1, 2, Some(PSK.to_vec()), PqcAuth::Require);
        let resp = handshake_auth(2, 1, Some(PSK.to_vec()), PqcAuth::Require);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        ini.pending_sk
            .lock()
            .insert(5, Zeroizing::new(kp.secret.clone()));

        // INIT autenticado (iniciador 1 → respondedor 2).
        resp.handle_init(&authed(TAG_INIT, 5, 1, 2, &kp.public), RecvAuth::Hmac);
        let ct = resp.resp_cache.lock().get(&5).cloned().unwrap().1;
        // RESP autenticado (respondedor 2 → iniciador 1).
        ini.handle_resp(&authed(TAG_RESP, 5, 2, 1, &ct), RecvAuth::Hmac);

        assert_eq!(*ini.store.get(5).unwrap(), *resp.store.get(5).unwrap());
    }

    #[tokio::test]
    async fn require_mode_rejects_forged_and_plaintext_init() {
        let resp = handshake_auth(2, 1, Some(PSK.to_vec()), PqcAuth::Require);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();

        // 1) INIT en claro bajo require → descartado, no encapsula.
        let mut plain = 9u32.to_be_bytes().to_vec();
        plain.extend_from_slice(&kp.public);
        resp.handle_init(&plain, RecvAuth::Plain);
        assert!(
            resp.store.get(9).is_none(),
            "plaintext INIT rechazado en require"
        );

        // 2) INIT autenticado con PSK equivocado → MAC inválido, descartado.
        let mut forged = 9u32.to_be_bytes().to_vec();
        forged.extend_from_slice(&kp.public);
        let bad = link_mac::tag(
            b"wrong-psk",
            TAG_INIT,
            9,
            1,
            2,
            &kp.public,
            "ml-kem-768",
            1024,
        );
        forged.extend_from_slice(&bad);
        resp.handle_init(&forged, RecvAuth::Hmac);
        assert!(
            resp.store.get(9).is_none(),
            "MAC inválido rechazado, época intacta"
        );

        // 3) INIT autenticado correcto → sí encapsula.
        resp.handle_init(&authed(TAG_INIT, 9, 1, 2, &kp.public), RecvAuth::Hmac);
        assert!(resp.store.get(9).is_some(), "INIT válido aceptado");
    }

    /// Handshake firmado con **ML-DSA** (modo `sign`, criptografía asimétrica
    /// post-cuántica): round-trip end to end + rechazo de firma inválida y de
    /// frame en claro. Cada extremo firma con SU seed y verifica con la clave
    /// pública del peer.
    #[tokio::test]
    async fn signed_handshake_round_trip_and_rejects() {
        use common::crypto::pqc_sign;
        let a = pqc_sign::keygen(); // identidad del nodo 1 (iniciador)
        let b = pqc_sign::keygen(); // identidad del nodo 2 (respondedor)

        let ini = handshake_full(
            1,
            2,
            None,
            PqcAuth::Sign,
            Some(a.secret_seed.to_vec()),
            Some(b.verifying_key.clone()),
        );
        let resp = handshake_full(
            2,
            1,
            None,
            PqcAuth::Sign,
            Some(b.secret_seed.to_vec()),
            Some(a.verifying_key.clone()),
        );
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        ini.pending_sk
            .lock()
            .insert(3, Zeroizing::new(kp.secret.clone()));

        // Construye un INIT firmado por el nodo 1 (sender=1, receiver=2).
        let signed_init = |epoch: u32, blob: &[u8]| {
            let mut p = epoch.to_be_bytes().to_vec();
            p.extend_from_slice(blob);
            let sig = pqc_sign::sign_handshake(
                &a.secret_seed,
                TAG_INIT,
                epoch,
                1,
                2,
                blob,
                "ml-kem-768",
                1024,
            )
            .unwrap();
            p.extend_from_slice(&sig);
            p
        };

        // 1) Frame en claro bajo modo sign → rechazado.
        let mut plain = 3u32.to_be_bytes().to_vec();
        plain.extend_from_slice(&kp.public);
        resp.handle_init(&plain, RecvAuth::Plain);
        assert!(
            resp.store.get(3).is_none(),
            "sign: frame en claro rechazado"
        );

        // 2) INIT firmado pero por la clave EQUIVOCADA (nodo b firmando como a)
        //    → firma inválida contra a.verifying_key → rechazado.
        let mut wrong = 3u32.to_be_bytes().to_vec();
        wrong.extend_from_slice(&kp.public);
        let bad_sig = pqc_sign::sign_handshake(
            &b.secret_seed,
            TAG_INIT,
            3,
            1,
            2,
            &kp.public,
            "ml-kem-768",
            1024,
        )
        .unwrap();
        wrong.extend_from_slice(&bad_sig);
        resp.handle_init(&wrong, RecvAuth::Signed);
        assert!(
            resp.store.get(3).is_none(),
            "sign: firma con clave equivocada rechazada"
        );

        // 3) INIT firmado correctamente → encapsula.
        resp.handle_init(&signed_init(3, &kp.public), RecvAuth::Signed);
        let ct = resp.resp_cache.lock().get(&3).cloned().unwrap().1;

        // RESP firmado por el nodo 2 (sender=2, receiver=1); el nodo 1 lo verifica
        // con b.verifying_key (su peer_verify_key).
        let mut resp_payload = 3u32.to_be_bytes().to_vec();
        resp_payload.extend_from_slice(&ct);
        let resp_sig =
            pqc_sign::sign_handshake(&b.secret_seed, TAG_RESP, 3, 2, 1, &ct, "ml-kem-768", 1024)
                .unwrap();
        resp_payload.extend_from_slice(&resp_sig);
        ini.handle_resp(&resp_payload, RecvAuth::Signed);

        // Ambos extremos comparten el mismo secreto, con handshake 100% firmado PQC.
        assert_eq!(*ini.store.get(3).unwrap(), *resp.store.get(3).unwrap());
    }

    #[tokio::test]
    async fn duplicate_init_keeps_same_secret_per_epoch() {
        let resp = handshake(2, 1);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        let mut init = 3u32.to_be_bytes().to_vec();
        init.extend_from_slice(&kp.public);

        resp.handle_init(&init, RecvAuth::Plain);
        let s1 = resp.store.get(3).unwrap();
        let ct1 = resp.resp_cache.lock().get(&3).cloned().unwrap().1;
        resp.handle_init(&init, RecvAuth::Plain); // duplicado
        let s2 = resp.store.get(3).unwrap();
        let ct2 = resp.resp_cache.lock().get(&3).cloned().unwrap().1;
        assert_eq!(*s1, *s2, "secret stable on duplicate INIT");
        assert_eq!(ct1, ct2, "cached ciphertext stable");
    }

    /// El iniciador se reinicia y repite una época con keypair NUEVO.
    ///
    /// El respondedor tenía cacheado el ciphertext viejo. Reenviarlo era
    /// silenciosamente catastrófico: ML-KEM aplica *implicit rejection* y el
    /// iniciador decapsula a un secreto pseudoaleatorio SIN error, así que
    /// los dos extremos acababan con secretos distintos para la misma época,
    /// ambos convencidos de haber cerrado el handshake. Como el enlace no
    /// lleva MAC, los frames se descifraban a basura y la basura llegaba
    /// hasta las claves que reciben los SAEs.
    #[tokio::test]
    async fn a_restarted_initiator_gets_a_fresh_encapsulation() {
        let epoch = 4u32;
        let resp = handshake(2, 1);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();

        // Primer arranque del iniciador.
        let kp_old = kem.keygen().unwrap();
        let mut init_old = epoch.to_be_bytes().to_vec();
        init_old.extend_from_slice(&kp_old.public);
        resp.handle_init(&init_old, RecvAuth::Plain);
        let s_old = resp.store.get(epoch).unwrap();

        // Se reinicia: keypair nuevo, misma época.
        let ini = handshake(1, 2);
        let kp_new = kem.keygen().unwrap();
        ini.pending_sk
            .lock()
            .insert(epoch, Zeroizing::new(kp_new.secret.clone()));
        let mut init_new = epoch.to_be_bytes().to_vec();
        init_new.extend_from_slice(&kp_new.public);
        resp.handle_init(&init_new, RecvAuth::Plain);

        let (cached_pk, ct_new) = resp.resp_cache.lock().get(&epoch).cloned().unwrap();
        assert_eq!(cached_pk, kp_new.public, "la caché sigue a la pubkey nueva");

        let mut resp_payload = epoch.to_be_bytes().to_vec();
        resp_payload.extend_from_slice(&ct_new);
        ini.handle_resp(&resp_payload, RecvAuth::Plain);

        let s_ini = ini.store.get(epoch).unwrap();
        let s_resp = resp.store.get(epoch).unwrap();
        assert_eq!(
            *s_ini, *s_resp,
            "tras el reinicio los dos extremos vuelven a compartir secreto",
        );
        assert_ne!(
            *s_resp, *s_old,
            "el respondedor descarta el secreto de la sesión anterior",
        );
    }

    #[test]
    fn role_split_is_deterministic() {
        assert!(handshake(1, 2).is_initiator());
        assert!(!handshake(2, 1).is_initiator());
    }

    #[tokio::test]
    async fn rekey_disabled_yields_single_epoch() {
        // n=0 y rekey_secs=0 ⇒ clock_disabled ⇒ tras 0..=lookahead, no rota.
        let hs = PqcHandshake::new(
            common::crypto::pqc::suite::ML_KEM_768.to_string(),
            1,
            2,
            "127.0.0.1:1".to_string(),
            Arc::new(PeerOut::new()),
            SecretStore::new(0, 0),
            RekeyClock::new(0),
            0,
            0,
            1024,
            None,
            PqcAuth::Off,
            None,
            None,
        );
        assert!(hs.clock_disabled());
        // (epoch_of sanity, para no dejar el import sin uso si se recorta arriba)
        assert_eq!(
            epoch_of(&[0, 0, 0, 5, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]),
            5
        );
    }
}
