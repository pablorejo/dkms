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
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use common::crypto::link_mac::{self, TAG_INIT, TAG_LEN, TAG_RESP, TAG_RESYNC};
use common::crypto::pqc_sign::{self, SIGNATURE_LEN};
use parking_lot::Mutex;
// `tokio::time::Instant` y no `std`: es el mismo reloj en producción y el
// reloj pausable de los tests del bucle de rotación.
use tokio::{sync::Notify, time::Instant};
use tracing::{debug, info, warn};
use wire::{
    Frame, FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_INIT_AUTH, FRAME_PQC_KEM_INIT_SIGNED,
    FRAME_PQC_KEM_RESP, FRAME_PQC_KEM_RESP_AUTH, FRAME_PQC_KEM_RESP_SIGNED, FRAME_PQC_RESYNC_REQ,
    FRAME_PQC_RESYNC_REQ_AUTH, FRAME_PQC_RESYNC_REQ_SIGNED,
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

/// Tope de un `establish`: pasado esto sin RESP se devuelve el control al
/// bucle de rotación, que es el mismo que atiende reconexiones y resyncs.
/// Sin tope, un peer mudo congelaba las tres cosas y el enlace parecía sano.
const ESTABLISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Cuánto espera el bucle antes de volver a intentar un `establish` que
/// agotó su tiempo.
const ESTABLISH_RETRY: Duration = Duration::from_secs(30);

/// Por dónde salen los frames de handshake y por dónde llega la señal de
/// reconexión. En producción es [`PeerOut`]; en los tests, un bucle en
/// proceso hacia el otro extremo, que es lo que permite probar el bucle de
/// rotación entero con el reloj de tokio pausado.
pub trait HandshakeTransport: Send + Sync {
    /// Encola un frame hacia el peer. `false` si no cupo.
    fn send(&self, peer_id: u32, peer_addr: &str, frame: Frame) -> bool;
    /// Señal que se dispara en cada RE-conexión con el peer.
    fn reconnect_signal(&self, peer_id: u32, peer_addr: &str) -> Arc<Notify>;
}

impl HandshakeTransport for PeerOut {
    fn send(&self, peer_id: u32, peer_addr: &str, frame: Frame) -> bool {
        PeerOut::send(self, peer_id, peer_addr, frame)
    }
    fn reconnect_signal(&self, peer_id: u32, peer_addr: &str) -> Arc<Notify> {
        PeerOut::reconnect_signal(self, peer_id, peer_addr)
    }
}

/// Coordinador del handshake ML-KEM (multi-época) de UN enlace PQC.
/// Identidad de **certificado de nodo** para el handshake firmado (modo
/// `sign`). Preferida sobre la semilla cruda `sign_seed`: la cadena viaja en
/// el propio handshake y el peer la verifica contra la CA de red, así no hay
/// que precargar la clave pública del vecino (que además obligaría a tocar a
/// todos los vecinos al renovar). Es el patrón que el ORR ya usa para su
/// anuncio (commit 28525cb).
#[derive(Clone)]
pub struct SignIdentity {
    /// Firma con la clave del cert de ESTE nodo (`tls.key_path`, ML-DSA-65).
    pub signer: Arc<pqc_sign::MlDsa65Signer>,
    /// Cadena de certs de este nodo (DER, hoja primero) que se adjunta.
    pub chain: Arc<Vec<Vec<u8>>>,
    /// CA de red para verificar la cadena del peer.
    pub roots: common::cert_identity::TrustRoots,
}

/// Serializa una cadena de certs DER: `u16(n) ‖ [u32(len) ‖ der]*`. Cadena
/// vacía (`n = 0`) marca el camino legacy (firma con `sign_seed`, verificación
/// con `peer_verify_key`).
fn encode_cert_chain(chain: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(chain.len() as u16).to_be_bytes());
    for c in chain {
        out.extend_from_slice(&(c.len() as u32).to_be_bytes());
        out.extend_from_slice(c);
    }
    out
}

/// Inverso de [`encode_cert_chain`]. Devuelve `(cadena, resto)`, donde `resto`
/// es lo que sigue a la cadena en el payload (el blob). `None` si está
/// truncado.
fn split_cert_chain(buf: &[u8]) -> Option<(Vec<Vec<u8>>, &[u8])> {
    let (n_bytes, mut rest) = buf.split_first_chunk::<2>()?;
    let n = u16::from_be_bytes(*n_bytes) as usize;
    let mut chain = Vec::with_capacity(n);
    for _ in 0..n {
        let (len_bytes, tail) = rest.split_first_chunk::<4>()?;
        let len = u32::from_be_bytes(*len_bytes) as usize;
        if tail.len() < len {
            return None;
        }
        chain.push(tail[..len].to_vec());
        rest = &tail[len..];
    }
    Some((chain, rest))
}

pub struct PqcHandshake {
    suite: String,
    my_id: u32,
    peer_id: u32,
    peer_addr: String,
    peer_out: Arc<dyn HandshakeTransport>,
    store: Arc<SecretStore>,
    /// Disparador de rotación (lo alimenta el `PqcKeySource` emisor).
    clock: Arc<RekeyClock>,
    /// Épocas pre-cargadas por delante de la activa.
    lookahead: u32,
    /// Tope de edad de una época en segundos (0 = sin disparo por tiempo).
    rekey_secs: u64,
    /// Decap keys del iniciador por época, vivas hasta que llega el RESP.
    pending_sk: Mutex<HashMap<u32, Zeroizing<Vec<u8>>>>,
    /// Pubkey de cada keypair pendiente, para reenviar el MISMO INIT cuando
    /// `establish` agotó su tiempo y se reintenta. Un keypair nuevo para la
    /// misma época dejaría en vuelo un RESP que decapsularíamos con la sk
    /// equivocada — implicit rejection: secreto distinto, sin error.
    pending_pk: Mutex<HashMap<u32, Vec<u8>>>,
    /// Frames de handshake que `peer_out` no encoló (cola del peer llena).
    handshake_drops: AtomicU64,
    /// Veces que `establish` agotó [`ESTABLISH_TIMEOUT`] sin RESP.
    establish_timeouts: AtomicU64,
    /// Rotaciones por reloj (tiempo o volumen) completadas por el bucle.
    rotations: AtomicU64,
    /// Por época, lo que el respondedor encapsuló: `(pubkey del iniciador,
    /// ciphertext)`. La pubkey va en la clave porque el ciphertext SOLO sirve
    /// para esa pubkey — ver [`PqcHandshake::handle_init`].
    resp_cache: Mutex<HashMap<u32, (Vec<u8>, Vec<u8>)>>,
    /// Último re-enlace, para el rate-limit de [`RELINK_MIN_INTERVAL`].
    last_relink: Mutex<Option<Instant>>,
    /// Última petición de resync enviada (respondedor), mismo rate-limit.
    last_resync_req: Mutex<Option<Instant>>,
    /// PSK del enlace para autenticar el handshake por HMAC (§Fase 5).
    /// `None` → sin PSK.
    psk: Option<Vec<u8>>,
    /// Semilla ML-DSA de firma de ESTE nodo (modo `sign`). `None` → no firma.
    sign_seed: Option<Vec<u8>>,
    /// Clave pública ML-DSA del PEER para verificar sus firmas (modo `sign`,
    /// camino legacy sin certificados).
    peer_verify_key: Option<Vec<u8>>,
    /// Identidad de certificado de nodo (modo `sign` moderno). Preferida sobre
    /// `sign_seed`/`peer_verify_key`. Ver [`SignIdentity`].
    cert_id: Option<SignIdentity>,
    /// Id esperado en el SAN del cert del peer: `qkc-<peer_id>`.
    peer_cert_id: String,
    /// Política de autenticación (off/prefer/require/sign).
    auth: PqcAuth,
    /// Tamaño de clave en bits, atado en el MAC/firma (cierra el mismatch).
    key_size_bits: u32,
}

/// Qué mensaje de handshake se envía: fija el kind del frame (claro / HMAC /
/// firmado) y el tag de dominio del MAC o la firma.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HsMsg {
    /// Iniciador → respondedor: `época ‖ pubkey`.
    Init,
    /// Respondedor → iniciador: `época ‖ ciphertext`.
    Resp,
    /// Respondedor → iniciador: «cifras con épocas que no tengo; mi ventana
    /// llega a `época`, renegocia por encima». Blob vacío.
    ResyncReq,
}

impl HsMsg {
    /// `(claro, hmac, firmado)`.
    fn kinds(self) -> (u8, u8, u8) {
        match self {
            HsMsg::Init => (
                FRAME_PQC_KEM_INIT,
                FRAME_PQC_KEM_INIT_AUTH,
                FRAME_PQC_KEM_INIT_SIGNED,
            ),
            HsMsg::Resp => (
                FRAME_PQC_KEM_RESP,
                FRAME_PQC_KEM_RESP_AUTH,
                FRAME_PQC_KEM_RESP_SIGNED,
            ),
            HsMsg::ResyncReq => (
                FRAME_PQC_RESYNC_REQ,
                FRAME_PQC_RESYNC_REQ_AUTH,
                FRAME_PQC_RESYNC_REQ_SIGNED,
            ),
        }
    }

    fn tag(self) -> &'static [u8] {
        match self {
            HsMsg::Init => TAG_INIT,
            HsMsg::Resp => TAG_RESP,
            HsMsg::ResyncReq => TAG_RESYNC,
        }
    }
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
        peer_out: Arc<dyn HandshakeTransport>,
        store: Arc<SecretStore>,
        clock: Arc<RekeyClock>,
        lookahead: u32,
        rekey_secs: u64,
        key_size_bits: u32,
        psk: Option<Vec<u8>>,
        auth: PqcAuth,
        sign_seed: Option<Vec<u8>>,
        peer_verify_key: Option<Vec<u8>>,
        cert_id: Option<SignIdentity>,
    ) -> Arc<Self> {
        let peer_cert_id = format!("qkc-{peer_id}");
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
            pending_pk: Mutex::new(HashMap::new()),
            handshake_drops: AtomicU64::new(0),
            establish_timeouts: AtomicU64::new(0),
            rotations: AtomicU64::new(0),
            resp_cache: Mutex::new(HashMap::new()),
            last_relink: Mutex::new(None),
            last_resync_req: Mutex::new(None),
            psk,
            sign_seed,
            peer_verify_key,
            cert_id,
            peer_cert_id,
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
    fn send_handshake(&self, msg: HsMsg, epoch: u32, blob: &[u8]) -> bool {
        let (plain, hmac_kind, signed_kind) = msg.kinds();
        let tag_kind = msg.tag();
        let mut payload = Vec::with_capacity(4 + blob.len() + SIGNATURE_LEN);
        payload.extend_from_slice(&epoch.to_be_bytes());
        payload.extend_from_slice(blob);

        let kind = if self.auth == PqcAuth::Sign {
            // Preferir el certificado de nodo (cadena verificable contra la CA)
            // sobre la semilla cruda con pubkey precargada (legacy). El payload
            // firmado es `época ‖ cadena ‖ blob ‖ firma`: la cadena va ANTES
            // del blob para poder delimitarla (el blob es el resto hasta la
            // firma). Cadena vacía = legacy.
            let signed = if let Some(id) = &self.cert_id {
                let sig = pqc_sign::sign_handshake_with(
                    &id.signer,
                    tag_kind,
                    epoch,
                    self.my_id,
                    self.peer_id,
                    blob,
                    &self.suite,
                    self.key_size_bits,
                );
                Some((encode_cert_chain(&id.chain), sig))
            } else if let Some(seed) = self.sign_seed.as_deref() {
                match pqc_sign::sign_handshake(
                    seed,
                    tag_kind,
                    epoch,
                    self.my_id,
                    self.peer_id,
                    blob,
                    &self.suite,
                    self.key_size_bits,
                ) {
                    Ok(sig) => Some((encode_cert_chain(&[]), sig)),
                    Err(e) => {
                        warn!(peer = self.peer_id, error = ?e, "qkc.pqc: fallo al firmar; envío en claro");
                        None
                    }
                }
            } else {
                warn!(
                    peer = self.peer_id,
                    "qkc.pqc: modo sign sin cert ni sign_secret_seed; envío en claro"
                );
                None
            };
            match signed {
                Some((chain_block, sig)) => {
                    // Reconstruir en el orden nuevo: época ‖ cadena ‖ blob ‖ firma.
                    let mut p = Vec::with_capacity(4 + chain_block.len() + blob.len() + sig.len());
                    p.extend_from_slice(&epoch.to_be_bytes());
                    p.extend_from_slice(&chain_block);
                    p.extend_from_slice(blob);
                    p.extend_from_slice(&sig);
                    payload = p;
                    signed_kind
                }
                None => plain,
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
        let ok = self.peer_out.send(self.peer_id, &self.peer_addr, f);
        if !ok {
            // Un INIT que no sale es indistinguible de uno que salió y no
            // tuvo respuesta, salvo por esto.
            let n = self.handshake_drops.fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    me = self.my_id,
                    peer = self.peer_id,
                    epoch,
                    kind,
                    dropped = n + 1,
                    "qkc.pqc: frame de handshake no encolado (cola hacia el peer llena)"
                );
            }
        }
        ok
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
                if payload.len() < 4 + SIGNATURE_LEN {
                    warn!(
                        peer = self.peer_id,
                        "qkc.pqc: payload firmado demasiado corto"
                    );
                    return None;
                }
                // época ‖ cadena ‖ blob ‖ firma
                let (rest, sig) = payload.split_at(payload.len() - SIGNATURE_LEN);
                let (epoch, after_epoch) = split_epoch(rest)?;
                let (chain, blob) = split_cert_chain(after_epoch)?;
                // La clave de verificación sale de la cadena (verificada contra
                // la CA de red) o, si la cadena viene vacía, de la pubkey
                // precargada (legacy). El SAN de la cadena debe decir
                // `qkc-<peer_id>`: así un miembro de la red con otro cert no
                // puede hacerse pasar por este vecino.
                let vk: Vec<u8> = if !chain.is_empty() {
                    let Some(roots) = self.cert_id.as_ref().map(|c| &c.roots) else {
                        warn!(
                            peer = self.peer_id,
                            "qkc.pqc: handshake con cadena de certs pero sin CA de red; descarto"
                        );
                        return None;
                    };
                    match common::cert_identity::verify_node_cert(&chain, roots, &self.peer_cert_id)
                    {
                        Ok(vk) => vk,
                        Err(e) => {
                            warn!(
                                peer = self.peer_id,
                                error = %e,
                                "qkc.pqc: cadena de certs del handshake rechazada; descarto"
                            );
                            return None;
                        }
                    }
                } else {
                    let Some(vk) = self.peer_verify_key.clone() else {
                        warn!(
                            peer = self.peer_id,
                            "qkc.pqc: frame firmado sin cadena y sin peer_verify_key; descarto"
                        );
                        return None;
                    };
                    vk
                };
                if pqc_sign::verify_handshake(
                    &vk,
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
            // separado, y puede PEDIRLO: manda un `FRAME_PQC_RESYNC_REQ` con
            // su ventana y el iniciador renegocia por encima de las dos.
            // Hasta 2026-08-30 aquí solo se logueaba, y una divergencia que
            // viera solo este lado exigía reiniciar a mano.
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let peer_epoch = me.store.resync_requested().await;
                    let mine = me.store.highest().unwrap_or(0);
                    {
                        let mut last = me.last_resync_req.lock();
                        if let Some(prev) = *last {
                            if prev.elapsed() < RELINK_MIN_INTERVAL {
                                debug!(
                                    peer = me.peer_id,
                                    "qkc.pqc.resync_req omitido (demasiado seguido)"
                                );
                                continue;
                            }
                        }
                        *last = Some(Instant::now());
                    }
                    warn!(
                        me = me.my_id,
                        peer = me.peer_id,
                        peer_epoch,
                        mia = ?(me.store.lowest(), me.store.highest()),
                        "qkc.pqc: el peer cifra con épocas que no tengo; le pido que renegocie \
                         por encima de mi ventana (resync_req)",
                    );
                    me.send_handshake(HsMsg::ResyncReq, mine, &[]);
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
            // Época más alta que queremos negociada: arranca en el lookahead,
            // cada rotación la sube en uno y un re-enlace la lleva a
            // `base + lookahead`.
            let mut target: u32 = me.lookahead;
            // Queda trabajo cuando un `establish` agotó su tiempo: se
            // reintenta pasado ESTABLISH_RETRY, atendiendo mientras tanto
            // reconexiones y resyncs.
            let mut need_work = true;
            // Poda pendiente de un re-enlace que no pudo completarse.
            let mut prune_after: Option<u32> = None;
            let mut last_rotation = Instant::now();
            // El temporizador vive FUERA del bucle. Reconstruirlo en cada
            // vuelta lo reiniciaba con cada reconexión o resync, y un enlace
            // con algún bache no llegaba a rotar nunca (una rotación en 15 h,
            // 2026-08-24). Solo el brazo de rotación lo rearma. Con la
            // rotación desactivada `wait_rotate` no resuelve jamás, que es
            // exactamente lo que queremos.
            let mut rotate = std::pin::pin!(me.clock.wait_rotate(me.rekey_secs));
            let mut retry = std::pin::pin!(tokio::time::sleep(Duration::ZERO));
            for epoch in 0..=me.lookahead {
                if !me.establish(epoch).await {
                    // Arranque sin peer: el bucle sigue intentándolo sin
                    // dejar de escuchar. Antes esto bloqueaba aquí para
                    // siempre.
                    retry.as_mut().set(tokio::time::sleep(ESTABLISH_RETRY));
                    break;
                }
                need_work = false;
            }
            loop {
                // Un único dueño de `establish`: este bucle. El re-enlace
                // NO puede ir en su propia task o dos `establish` de la
                // misma época pisarían `pending_sk` y el RESP se
                // decapsularía con la sk equivocada.
                tokio::select! {
                    biased;
                    _ = &mut retry, if need_work => {
                        if me.establish_up_to(target).await {
                            need_work = false;
                            if let Some(base) = prune_after.take() {
                                let dropped = me.prune_below(base);
                                info!(me = me.my_id, peer = me.peer_id, base, epocas_descartadas = dropped,
                                      "qkc.pqc.relink completado en el reintento");
                            }
                        } else {
                            retry.as_mut().set(tokio::time::sleep(ESTABLISH_RETRY));
                        }
                    }
                    trigger = &mut rotate => {
                        let next = me.store.highest().map_or(target, |h| h.saturating_add(1));
                        target = target.max(next);
                        let n = me.rotations.fetch_add(1, Ordering::Relaxed);
                        // La línea que cuenta el soak: una por rotación, con
                        // el intervalo real desde la anterior.
                        info!(
                            me = me.my_id,
                            peer = me.peer_id,
                            epoch = target,
                            rotation = n + 1,
                            since_last_s = last_rotation.elapsed().as_secs(),
                            trigger = ?trigger,
                            "qkc.pqc.rotation"
                        );
                        last_rotation = Instant::now();
                        rotate.as_mut().set(me.clock.wait_rotate(me.rekey_secs));
                        need_work = true;
                        retry.as_mut().set(tokio::time::sleep(Duration::ZERO));
                    }
                    _ = reconnect.notified() => {
                        if let Some((aim, complete)) = me.relink(0).await {
                            target = target.max(aim);
                            if !complete {
                                prune_after = Some(aim.saturating_sub(me.lookahead));
                                need_work = true;
                                retry.as_mut().set(tokio::time::sleep(ESTABLISH_RETRY));
                            }
                        }
                    }
                    // El lado DEC ha visto al peer cifrar con épocas que no
                    // tenemos. Es la única forma de enterarse: las ventanas de
                    // los dos extremos pueden separarse tras un reinicio y
                    // nadie lo nota hasta que llega un frame indescifrable.
                    peer_epoch = me.store.resync_requested() => {
                        if let Some((aim, complete)) = me.relink(peer_epoch).await {
                            target = target.max(aim);
                            if !complete {
                                prune_after = Some(aim.saturating_sub(me.lookahead));
                                need_work = true;
                                retry.as_mut().set(tokio::time::sleep(ESTABLISH_RETRY));
                            }
                        }
                    }
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
    async fn relink(&self, peer_epoch: u32) -> Option<(u32, bool)> {
        let now = Instant::now();
        {
            let mut last = self.last_relink.lock();
            if let Some(prev) = *last {
                if now.duration_since(prev) < RELINK_MIN_INTERVAL {
                    debug!(
                        peer = self.peer_id,
                        "qkc.pqc.relink omitido (demasiado seguido)"
                    );
                    return None;
                }
            }
            *last = Some(now);
        }
        let mine = self.store.highest().map(|h| h + 1).unwrap_or(0);
        let base = mine.max(peer_epoch.saturating_add(1));
        let aim = base.saturating_add(self.lookahead);
        warn!(
            me = self.my_id,
            peer = self.peer_id,
            base,
            peer_epoch,
            "qkc.pqc.relink: renegocio el enlace (reconexión del peer o épocas suyas que no tengo)",
        );
        let mut complete = true;
        for epoch in base..=aim {
            if !self.establish(epoch).await {
                complete = false;
                break;
            }
        }
        if complete {
            // Podamos DESPUÉS de negociar: si vaciáramos antes, `enc_keys` se
            // quedaría sin ninguna época mientras dura el handshake.
            let dropped = self.prune_below(base);
            info!(
                me = self.my_id,
                peer = self.peer_id,
                base,
                epocas_descartadas = dropped,
                "qkc.pqc.relink completado",
            );
        } else {
            warn!(
                me = self.my_id,
                peer = self.peer_id,
                base,
                aim,
                "qkc.pqc.relink incompleto: el peer no contesta; el bucle reintenta y poda al terminar",
            );
        }
        Some((aim, complete))
    }

    /// Poda las épocas por debajo de `base` en el store y en los pendientes.
    fn prune_below(&self, base: u32) -> usize {
        let dropped = self.store.prune_below(base);
        self.pending_sk.lock().retain(|e, _| *e >= base);
        self.pending_pk.lock().retain(|e, _| *e >= base);
        self.resp_cache.lock().retain(|e, _| *e >= base);
        dropped
    }

    /// Establece, en orden, todas las épocas que faltan hasta `target`.
    /// `false` si alguna agotó su tiempo (las anteriores quedan hechas).
    async fn establish_up_to(&self, target: u32) -> bool {
        let from = self.store.highest().map_or(0, |h| h.saturating_add(1));
        for epoch in from..=target {
            if !self.establish(epoch).await {
                return false;
            }
        }
        true
    }

    /// Sin volumen (`n == 0`) ni tiempo (`rekey_secs == 0`) no hay rotación:
    /// `wait_rotate` queda pendiente para siempre y el bucle solo atiende
    /// reconexiones y resyncs.
    #[cfg(test)]
    fn clock_disabled(&self) -> bool {
        self.rekey_secs == 0 && self.clock.is_volume_disabled()
    }

    /// Iniciador: establece el secreto de `epoch` (idempotente). Genera el
    /// keypair —o reutiliza el de un intento anterior que agotó su tiempo—,
    /// reenvía INIT{epoch,pk} cada [`INIT_RETRY`] hasta que llega el RESP y
    /// `store` tiene la época, o hasta [`ESTABLISH_TIMEOUT`]. Devuelve si la
    /// época quedó establecida.
    ///
    /// Acotado a propósito: este es el único dueño del handshake y el bucle
    /// que lo llama es el mismo que atiende reconexiones y resyncs. Un
    /// `establish` sin límite contra un peer mudo congelaba las tres cosas y
    /// se veía desde fuera como un enlace sano que nunca rota.
    async fn establish(&self, epoch: u32) -> bool {
        if self.store.contains(epoch) {
            return true;
        }
        let pubkey = {
            let reuse = self.pending_sk.lock().contains_key(&epoch);
            let pk = if reuse {
                self.pending_pk.lock().get(&epoch).cloned()
            } else {
                None
            };
            match pk {
                Some(pk) => pk,
                None => {
                    let kem = match common::crypto::pqc::kem_for(&self.suite) {
                        Ok(k) => k,
                        Err(e) => {
                            warn!(peer = self.peer_id, error = %e, "qkc.pqc: kem_for failed");
                            return false;
                        }
                    };
                    let kp = match kem.keygen() {
                        Ok(k) => k,
                        Err(e) => {
                            warn!(peer = self.peer_id, error = %e, "qkc.pqc: keygen failed");
                            return false;
                        }
                    };
                    self.pending_sk
                        .lock()
                        .insert(epoch, Zeroizing::new(kp.secret));
                    self.pending_pk.lock().insert(epoch, kp.public.clone());
                    kp.public
                }
            }
        };
        let started = Instant::now();
        let mut attempts: u64 = 0;
        while !self.store.contains(epoch) {
            if started.elapsed() >= ESTABLISH_TIMEOUT {
                let n = self.establish_timeouts.fetch_add(1, Ordering::Relaxed);
                warn!(
                    me = self.my_id,
                    peer = self.peer_id,
                    epoch,
                    attempts,
                    timeouts = n + 1,
                    handshake_drops = self.handshake_drops.load(Ordering::Relaxed),
                    "qkc.pqc.establish: sin RESP del peer en {} s; devuelvo el control al bucle \
                     (reconexiones y resyncs) y reintento en {} s",
                    ESTABLISH_TIMEOUT.as_secs(),
                    ESTABLISH_RETRY.as_secs(),
                );
                return false;
            }
            self.send_handshake(HsMsg::Init, epoch, &pubkey);
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
        self.pending_pk.lock().remove(&epoch);
        true
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
                self.send_handshake(HsMsg::Resp, epoch, &ct);
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
        self.send_handshake(HsMsg::Resp, epoch, &encap.ciphertext);
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

    /// Iniciador: el respondedor pide renegociar por encima de `peer_epoch`
    /// (su ventana). Pasa por [`Self::accept`] como cualquier otro mensaje de
    /// handshake —bajo `require`/`sign` una petición sin autenticar se
    /// descarta— y desemboca en el brazo de resync del bucle de rotación,
    /// que es el único dueño de `establish`. En el respondedor no hace nada:
    /// solo el lex-menor renegocia.
    pub fn handle_resync_request(&self, payload: &[u8], recv: RecvAuth) {
        let Some((peer_epoch, _blob)) = self.accept(TAG_RESYNC, payload, recv) else {
            return;
        };
        if !self.is_initiator() {
            debug!(
                me = self.my_id,
                peer = self.peer_id,
                peer_epoch,
                "qkc.pqc: petición de resync recibida en el respondedor; ignorada"
            );
            return;
        }
        info!(
            me = self.my_id,
            peer = self.peer_id,
            peer_epoch,
            mia = ?(self.store.lowest(), self.store.highest()),
            "qkc.pqc: el respondedor pide resincronizar; renegocio por encima de su ventana",
        );
        // `request_resync` toma el 0 como "nada pendiente": una ventana que
        // llega a 0 pide igual, y el re-enlace sube de todos modos por
        // encima de la nuestra.
        self.store.request_resync(peer_epoch.max(1));
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
        handshake_full(my_id, peer_id, psk, auth, None, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn handshake_full(
        my_id: u32,
        peer_id: u32,
        psk: Option<Vec<u8>>,
        auth: PqcAuth,
        sign_seed: Option<Vec<u8>>,
        peer_verify_key: Option<Vec<u8>>,
        cert_id: Option<SignIdentity>,
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
            cert_id,
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
            None,
        );
        let resp = handshake_full(
            2,
            1,
            None,
            PqcAuth::Sign,
            Some(b.secret_seed.to_vec()),
            Some(a.verifying_key.clone()),
            None,
        );
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        ini.pending_sk
            .lock()
            .insert(3, Zeroizing::new(kp.secret.clone()));

        // Construye un INIT firmado por el nodo 1 (sender=1, receiver=2).
        let signed_init = |epoch: u32, blob: &[u8]| {
            let mut p = epoch.to_be_bytes().to_vec();
            p.extend_from_slice(&[0, 0]); // cadena de certs vacía (camino legacy)
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
        wrong.extend_from_slice(&[0, 0]); // cadena vacía
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
        resp_payload.extend_from_slice(&[0, 0]); // cadena vacía
        resp_payload.extend_from_slice(&ct);
        let resp_sig =
            pqc_sign::sign_handshake(&b.secret_seed, TAG_RESP, 3, 2, 1, &ct, "ml-kem-768", 1024)
                .unwrap();
        resp_payload.extend_from_slice(&resp_sig);
        ini.handle_resp(&resp_payload, RecvAuth::Signed);

        // Ambos extremos comparten el mismo secreto, con handshake 100% firmado PQC.
        assert_eq!(*ini.store.get(3).unwrap(), *resp.store.get(3).unwrap());
    }

    /// Handshake atado a **certificados de nodo** (el camino moderno): el
    /// respondedor verifica la cadena del iniciador contra la CA de red y el
    /// SAN `qkc-<peer_id>`, en vez de una pubkey precargada. Cubre: cadena
    /// buena → acepta; CA ajena → rechaza; SAN equivocado → rechaza.
    #[tokio::test]
    async fn cert_bound_handshake_accepts_valid_chain_and_rejects_impostors() {
        use common::crypto::pqc_sign::{self, MlDsa65Signer};
        let dir = std::env::temp_dir().join(format!("qkc_hs_cert_{}", std::process::id()));
        let Some(pki) = common::test_support::mldsa_test_pki(&dir, &["qkc-1", "qkc-99"]) else {
            common::test_support::skip_or_fail("openssl sin ML-DSA (<3.5)");
            return;
        };
        let net_roots =
            common::cert_identity::TrustRoots::from_pem(&std::fs::read(&pki.ca_crt).unwrap())
                .unwrap();
        let sign_id = |id: &str, roots: common::cert_identity::TrustRoots| SignIdentity {
            signer: Arc::new(
                MlDsa65Signer::from_pkcs8_pem(&std::fs::read(pki.key(id)).unwrap()).unwrap(),
            ),
            chain: Arc::new(vec![pki.cert_der(id)]),
            roots,
        };

        // Respondedor (nodo 2) que espera que su peer, el nodo 1, presente un
        // cert de la net-CA con SAN `qkc-1`.
        let resp = handshake_full(
            2,
            1,
            None,
            PqcAuth::Sign,
            None,
            None,
            Some(sign_id("qkc-99", net_roots.clone())), // su propia identidad; da igual cuál
        );
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();

        // INIT firmado con un cert de nodo (sender=1, receiver=2). `signer_id`
        // es el cert que firma; `chain_id` la cadena que se adjunta.
        let make_init = |signer_id: &str, chain_id: &str| {
            let signer =
                MlDsa65Signer::from_pkcs8_pem(&std::fs::read(pki.key(signer_id)).unwrap()).unwrap();
            let sig = pqc_sign::sign_handshake_with(
                &signer,
                TAG_INIT,
                3,
                1,
                2,
                &kp.public,
                "ml-kem-768",
                1024,
            );
            let mut p = 3u32.to_be_bytes().to_vec();
            p.extend_from_slice(&encode_cert_chain(&[pki.cert_der(chain_id)]));
            p.extend_from_slice(&kp.public);
            p.extend_from_slice(&sig);
            p
        };

        // 1) Cadena buena (qkc-1) → aceptado.
        resp.handle_init(&make_init("qkc-1", "qkc-1"), RecvAuth::Signed);
        assert!(
            resp.resp_cache.lock().contains_key(&3),
            "cert de qkc-1 válido: aceptado"
        );

        // 2) SAN equivocado: qkc-99 es de la net-CA pero no es el peer esperado.
        let resp2 = handshake_full(
            2,
            1,
            None,
            PqcAuth::Sign,
            None,
            None,
            Some(sign_id("qkc-99", net_roots.clone())),
        );
        resp2.handle_init(&make_init("qkc-99", "qkc-99"), RecvAuth::Signed);
        assert!(
            resp2.resp_cache.lock().is_empty(),
            "SAN qkc-99 ≠ peer esperado qkc-1: rechazado"
        );

        // 3) CA ajena: un cert de "qkc-1" emitido por otra CA no verifica.
        let rogue_dir = dir.join("rogue");
        let rogue = common::test_support::mldsa_test_pki(&rogue_dir, &["qkc-1"]).unwrap();
        let resp3 = handshake_full(
            2,
            1,
            None,
            PqcAuth::Sign,
            None,
            None,
            Some(sign_id("qkc-99", net_roots)), // roots = net-CA
        );
        let rogue_signer =
            MlDsa65Signer::from_pkcs8_pem(&std::fs::read(rogue.key("qkc-1")).unwrap()).unwrap();
        let rogue_sig = pqc_sign::sign_handshake_with(
            &rogue_signer,
            TAG_INIT,
            3,
            1,
            2,
            &kp.public,
            "ml-kem-768",
            1024,
        );
        let mut rogue_init = 3u32.to_be_bytes().to_vec();
        rogue_init.extend_from_slice(&encode_cert_chain(&[rogue.cert_der("qkc-1")]));
        rogue_init.extend_from_slice(&kp.public);
        rogue_init.extend_from_slice(&rogue_sig);
        resp3.handle_init(&rogue_init, RecvAuth::Signed);
        assert!(
            resp3.resp_cache.lock().is_empty(),
            "cadena de una CA ajena: rechazada"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
            None,
        );
        assert!(hs.clock_disabled());
        // (epoch_of sanity, para no dejar el import sin uso si se recorta arriba)
        assert_eq!(
            epoch_of(&[0, 0, 0, 5, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]),
            5
        );
    }

    // ─── Bucle de rotación, con el reloj de tokio pausado ─────────────

    /// Transporte en proceso: lo que un extremo manda se entrega al otro en
    /// la misma llamada. `deliver = false` traga los frames (peer caído);
    /// `reject_first = n` hace que las primeras `n` colas "estén llenas".
    struct Loopback {
        other: Mutex<Option<Arc<PqcHandshake>>>,
        deliver: std::sync::atomic::AtomicBool,
        reject_first: AtomicU64,
        reconnect: Arc<Notify>,
    }

    impl Loopback {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                other: Mutex::new(None),
                deliver: std::sync::atomic::AtomicBool::new(true),
                reject_first: AtomicU64::new(0),
                reconnect: Arc::new(Notify::new()),
            })
        }
    }

    impl HandshakeTransport for Loopback {
        fn send(&self, _peer_id: u32, _peer_addr: &str, frame: Frame) -> bool {
            if self.reject_first.load(Ordering::Relaxed) > 0 {
                self.reject_first.fetch_sub(1, Ordering::Relaxed);
                return false;
            }
            if !self.deliver.load(Ordering::Relaxed) {
                return true;
            }
            let other = self.other.lock().clone();
            if let Some(o) = other {
                match frame.kind {
                    FRAME_PQC_KEM_INIT => o.handle_init(&frame.payload, RecvAuth::Plain),
                    FRAME_PQC_KEM_RESP => o.handle_resp(&frame.payload, RecvAuth::Plain),
                    FRAME_PQC_RESYNC_REQ => {
                        o.handle_resync_request(&frame.payload, RecvAuth::Plain)
                    }
                    _ => {}
                }
            }
            true
        }
        fn reconnect_signal(&self, _peer_id: u32, _peer_addr: &str) -> Arc<Notify> {
            self.reconnect.clone()
        }
    }

    /// Iniciador (1) y respondedor (2) unidos por dos `Loopback`; se devuelve
    /// el del iniciador, que es donde se simulan caídas y reconexiones.
    fn linked_pair(rekey_secs: u64) -> (Arc<PqcHandshake>, Arc<PqcHandshake>, Arc<Loopback>) {
        let to_resp = Loopback::new();
        let to_ini = Loopback::new();
        let mk = |my: u32, peer: u32, out: Arc<Loopback>| {
            PqcHandshake::new(
                common::crypto::pqc::suite::ML_KEM_768.to_string(),
                my,
                peer,
                "127.0.0.1:1".to_string(),
                out,
                SecretStore::new(2, 1000),
                RekeyClock::new(0),
                2,
                rekey_secs,
                1024,
                None,
                PqcAuth::Off,
                None,
                None,
                None,
            )
        };
        let ini = mk(1, 2, to_resp.clone());
        let resp = mk(2, 1, to_ini.clone());
        *to_resp.other.lock() = Some(resp.clone());
        *to_ini.other.lock() = Some(ini.clone());
        (ini, resp, to_resp)
    }

    /// Un enlace sin tráfico rota igual: el disparo por tiempo no depende de
    /// que pase nada más por el enlace.
    #[tokio::test(start_paused = true)]
    async fn rotation_fires_on_idle_link() {
        let (ini, resp, _t) = linked_pair(100);
        ini.spawn_rotation();
        tokio::time::sleep(Duration::from_secs(1005)).await;
        assert_eq!(ini.rotations.load(Ordering::Relaxed), 10);
        // 0..=2 al arrancar y una época más por rotación.
        assert_eq!(ini.store.highest(), Some(12));
        assert_eq!(resp.store.highest(), Some(12));
        assert_eq!(ini.establish_timeouts.load(Ordering::Relaxed), 0);
    }

    /// El soak de 15 h del 2026-08-24: una rotación en vez de catorce. Con el
    /// temporizador reconstruido en cada vuelta del `select!`, cada reconexión
    /// o resync reiniciaba la hora. Tres horas con un rebote cada diez minutos
    /// tienen que dar tres rotaciones, no cero.
    #[tokio::test(start_paused = true)]
    async fn time_rotation_survives_spurious_wakeups() {
        let (ini, resp, t) = linked_pair(3600);
        ini.spawn_rotation();
        for _ in 0..18 {
            tokio::time::sleep(Duration::from_secs(600)).await;
            t.reconnect.notify_one();
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(
            ini.rotations.load(Ordering::Relaxed),
            3,
            "3 h → 3 rotaciones aunque el enlace rebote cada 10 min"
        );
        assert_eq!(ini.store.highest(), resp.store.highest());
        assert!(ini.store.highest().unwrap() >= 2 + 3);
    }

    /// Un peer mudo no congela el bucle: `establish` agota su tiempo, el
    /// bucle vuelve a escuchar, y la reconexión que llega después se
    /// atiende. Antes se quedaba dentro de `establish` para siempre.
    #[tokio::test(start_paused = true)]
    async fn establish_timeout_keeps_loop_responsive() {
        let (ini, resp, t) = linked_pair(3600);
        t.deliver.store(false, Ordering::Relaxed);
        ini.spawn_rotation();
        tokio::time::sleep(ESTABLISH_TIMEOUT + Duration::from_secs(5)).await;
        assert_eq!(ini.establish_timeouts.load(Ordering::Relaxed), 1);
        assert_eq!(ini.store.highest(), None);

        // El peer vuelve: la señal de reconexión se atiende y el re-enlace
        // negocia por encima de las dos ventanas (base = 1 con peer_epoch 0).
        t.deliver.store(true, Ordering::Relaxed);
        t.reconnect.notify_one();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(ini.store.highest(), Some(3));
        assert_eq!(resp.store.highest(), Some(3));
        assert!(
            ini.pending_pk.lock().is_empty(),
            "nada pendiente tras el re-enlace"
        );

        // El reintento que quedó programado no rompe nada.
        tokio::time::sleep(ESTABLISH_RETRY + Duration::from_secs(5)).await;
        assert_eq!(ini.store.highest(), Some(3));
        assert_eq!(ini.establish_timeouts.load(Ordering::Relaxed), 1);
    }

    /// Un INIT que no cabe en la cola se cuenta y se reintenta con el MISMO
    /// keypair, y la época acaba establecida.
    #[tokio::test(start_paused = true)]
    async fn dropped_init_is_counted_and_retried() {
        let (ini, resp, t) = linked_pair(3600);
        t.reject_first.store(3, Ordering::Relaxed);
        ini.spawn_rotation();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert_eq!(ini.handshake_drops.load(Ordering::Relaxed), 3);
        assert_eq!(ini.store.highest(), Some(2));
        assert_eq!(resp.store.highest(), Some(2));
        assert!(ini.pending_pk.lock().is_empty(), "sin keypairs colgando");
        assert_eq!(*ini.store.get(0).unwrap(), *resp.store.get(0).unwrap());
    }

    /// El respondedor no puede mandar INIT, pero sí PEDIR: su lado DEC ve al
    /// iniciador cifrar con épocas que no tiene, manda 0x28 con su ventana y
    /// el iniciador renegocia por encima de las dos. Hasta ahora solo lo
    /// logueaba y una divergencia vista solo desde este lado exigía reinicio.
    #[tokio::test(start_paused = true)]
    async fn a_responder_resync_request_makes_the_initiator_relink() {
        let (ini, resp, _t) = linked_pair(3600);
        ini.spawn_rotation();
        resp.spawn_rotation();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(ini.store.highest(), Some(2));
        // Lo que hace el DEC del respondedor al no poder descifrar una época.
        resp.store.request_resync(9);
        tokio::time::sleep(Duration::from_secs(10)).await;
        let h = ini.store.highest().expect("épocas");
        assert!(h > 2, "el iniciador renegoció un bloque nuevo: highest={h}");
        assert_eq!(resp.store.highest(), Some(h));
        assert_eq!(*ini.store.get(h).unwrap(), *resp.store.get(h).unwrap());
        assert!(
            ini.store.get(0).is_none(),
            "la ventana vieja se podó tras el re-enlace"
        );
    }

    /// La petición pasa por la misma política que INIT/RESP: bajo `require`
    /// una en claro se descarta y una con el MAC del enlace llega al bucle.
    #[tokio::test(start_paused = true)]
    async fn a_resync_request_is_subject_to_the_handshake_policy() {
        let ini = handshake_auth(1, 2, Some(PSK.to_vec()), PqcAuth::Require);
        ini.handle_resync_request(&9u32.to_be_bytes(), RecvAuth::Plain);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), ini.store.resync_requested())
                .await
                .is_err(),
            "en claro bajo require no vale"
        );
        ini.handle_resync_request(&authed(TAG_RESYNC, 9, 2, 1, &[]), RecvAuth::Hmac);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), ini.store.resync_requested())
                .await
                .expect("la autenticada llega"),
            9
        );
    }

    /// Solo el lex-menor renegocia: un respondedor que reciba una petición la
    /// ignora en vez de intentar un INIT que rompería la invariante.
    #[tokio::test(start_paused = true)]
    async fn the_responder_ignores_an_incoming_resync_request() {
        let resp = handshake(2, 1);
        resp.handle_resync_request(&9u32.to_be_bytes(), RecvAuth::Plain);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), resp.store.resync_requested())
                .await
                .is_err()
        );
    }
}
