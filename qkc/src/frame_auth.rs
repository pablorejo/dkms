//! Estado por enlace de la autenticación de frames de datos.
//!
//! Envuelve `common::crypto::frame_mac` con lo que hace falta en caliente: la
//! sesión propia, el contador monotónico de salida y la ventana anti-replay de
//! entrada. Un `LinkFrameAuth` por enlace y sentido lógico — el mismo objeto
//! firma lo que sale y verifica lo que entra, porque la raíz (`link_psk`) es
//! simétrica y la sesión distingue quién habla.
//!
//! ## Orden de las comprobaciones
//!
//! [`LinkFrameAuth::open`] verifica el MAC **antes** de tocar la ventana. Al
//! revés, cualquiera podría tirar la ventana del receptor mandando basura con
//! un `session` inventado; con el MAC delante hace falta la raíz del enlace
//! para llegar siquiera a proponer una sesión nueva.

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use common::crypto::frame_mac::{self, FrameAad, ReplayWindow, DEFAULT_WINDOW};
use parking_lot::Mutex;
use rand::RngCore;
use tracing::{info, warn};
use wire::{Frame, AUTH_TRAILER_LEN};
use zeroize::Zeroizing;

use crate::config::FrameAuth;
use crate::pqc_source::SecretStore;

/// Por qué se ha rechazado un frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FrameAuthError {
    #[error("frame autenticado sin trailer completo")]
    Truncated,
    #[error("MAC de frame inválido")]
    BadMac,
    #[error("{0}")]
    Replay(#[from] frame_mac::ReplayError),
    #[error("frame en claro en un enlace con frame_auth = require")]
    PlaintextRejected,
}

/// Contadores para la línea `qkc.frame_auth` y para las pruebas.
#[derive(Debug, Default)]
pub struct FrameAuthStats {
    pub signed: AtomicU64,
    pub verified: AtomicU64,
    pub bad_mac: AtomicU64,
    pub replayed: AtomicU64,
    pub plaintext_accepted: AtomicU64,
    pub plaintext_rejected: AtomicU64,
}

/// De dónde sale la raíz del MAC de frames.
enum RootSource {
    /// `link_psk`: raíz simétrica fija, compartida por config. La `session` es
    /// una encarnación aleatoria por arranque (distingue reinicios de un
    /// replay).
    Fixed {
        root: Zeroizing<Vec<u8>>,
        session: u64,
        send_key: Zeroizing<[u8; 32]>,
    },
    /// Secreto del enlace PQC: la raíz es el secreto de la ÉPOCA del enlace
    /// (que ya rota sola con el handshake) y la `session` del frame ES esa
    /// época. No hay PSK que repartir — la autenticación la hereda del
    /// handshake, que va firmado con el certificado de nodo. Cada rotación
    /// cambia la raíz; el receptor deriva con el secreto de la época que trae
    /// el propio frame. Si aún no tiene ese secreto (rotación en vuelo), el
    /// frame se descarta y se regenera, nunca se acepta sin verificar.
    PerEpoch {
        store: Arc<SecretStore>,
        /// Caché del emisor: `(época, clave)`. La época de envío es `highest`.
        send_cache: Mutex<Option<(u32, Zeroizing<[u8; 32]>)>>,
    },
}

pub struct LinkFrameAuth {
    mode: FrameAuth,
    peer_id: u32,
    /// Fuente de la raíz del MAC: PSK fija o secreto de época del enlace.
    src: RootSource,
    /// Contador monotónico de salida. El primer frame lleva 1.
    counter: AtomicU64,
    /// Ventana anti-replay de entrada y clave cacheada de la sesión del peer.
    /// Un solo Mutex para las dos: se tocan juntas y sólo en el camino de
    /// verificación, que ya es serie por frame.
    recv: Mutex<RecvState>,
    pub stats: Arc<FrameAuthStats>,
}

struct RecvState {
    window: ReplayWindow,
    /// `(sesión del peer, clave derivada)` — evita un HKDF por frame.
    cached: Option<(u64, Zeroizing<[u8; 32]>)>,
}

impl LinkFrameAuth {
    /// `None` si el enlace no tiene raíz (`link_psk`): sin ella no hay nada que
    /// calcular. **El modo no decide si se construye**, sólo qué se firma: el
    /// NOTIFY se autentica siempre que haya raíz (es el plano de control del
    /// enlace, y así se comportaba desde 2026-08-27), mientras que los frames de
    /// datos siguen `mode`. En `require` sin PSK el caller debe fallar el
    /// arranque: correr sin autenticar creyendo que sí es justo lo que el flag
    /// existe para impedir.
    pub fn new(mode: FrameAuth, peer_id: u32, root: Option<Vec<u8>>) -> Option<Self> {
        let root = root?;
        let mut session_bytes = [0u8; 8];
        rand::thread_rng().fill_bytes(&mut session_bytes);
        // La sesión 0 es un valor válido, pero reservarla como "sin sesión" en
        // los logs cuesta nada y evita confusiones al leerlos.
        let session = u64::from_be_bytes(session_bytes).max(1);
        let send_key = Zeroizing::new(frame_mac::derive_key(&root, session));
        info!(
            peer = peer_id,
            ?mode,
            session,
            root = "psk",
            "qkc.frame_auth.enabled"
        );
        Some(Self::with_source(
            mode,
            peer_id,
            RootSource::Fixed {
                root: Zeroizing::new(root),
                session,
                send_key,
            },
        ))
    }

    /// Como [`new`](Self::new) pero derivando la raíz del **secreto del enlace
    /// PQC** (ver [`RootSource::PerEpoch`]): no necesita `link_psk`, la
    /// autenticación la hereda del handshake firmado con el cert. El sello se
    /// activa en cuanto el handshake establece la primera época.
    pub fn per_epoch(mode: FrameAuth, peer_id: u32, store: Arc<SecretStore>) -> Self {
        info!(
            peer = peer_id,
            ?mode,
            root = "pqc-epoch",
            "qkc.frame_auth.enabled"
        );
        Self::with_source(
            mode,
            peer_id,
            RootSource::PerEpoch {
                store,
                send_cache: Mutex::new(None),
            },
        )
    }

    fn with_source(mode: FrameAuth, peer_id: u32, src: RootSource) -> Self {
        Self {
            mode,
            peer_id,
            src,
            counter: AtomicU64::new(0),
            recv: Mutex::new(RecvState {
                window: ReplayWindow::new(DEFAULT_WINDOW),
                cached: None,
            }),
            stats: Arc::new(FrameAuthStats::default()),
        }
    }

    pub fn mode(&self) -> FrameAuth {
        self.mode
    }

    /// Sesión de salida actual (para el log). Con PSK es la encarnación fija;
    /// con secreto de época es la época activa (0 si aún no hay ninguna).
    pub fn session(&self) -> u64 {
        match &self.src {
            RootSource::Fixed { session, .. } => *session,
            RootSource::PerEpoch { store, .. } => store.highest().map(u64::from).unwrap_or(0),
        }
    }

    /// Material de salida `(session, clave)`. `None` en `PerEpoch` si el
    /// handshake aún no estableció ninguna época (el enlace tampoco tendría
    /// material OTP que enviar, así que no se llega a sellar en la práctica).
    fn send_material(&self) -> Option<(u64, Zeroizing<[u8; 32]>)> {
        match &self.src {
            RootSource::Fixed {
                session, send_key, ..
            } => Some((*session, send_key.clone())),
            RootSource::PerEpoch { store, send_cache } => {
                let epoch = store.highest()?;
                let mut c = send_cache.lock();
                if let Some((e, k)) = &*c {
                    if *e == epoch {
                        return Some((u64::from(epoch), k.clone()));
                    }
                }
                let secret = store.get(epoch)?;
                let k = Zeroizing::new(frame_mac::derive_key(secret.as_ref(), u64::from(epoch)));
                *c = Some((epoch, k.clone()));
                Some((u64::from(epoch), k))
            }
        }
    }

    /// Espera (solo en el arranque) a que haya material de sello de salida:
    /// en `PerEpoch`, la primera época del handshake; en `Fixed`, inmediato.
    /// Evita que los primeros NOTIFY salgan en claro — el receptor con raíz
    /// los descarta fail-closed y sus claves quedan huérfanas hasta que el
    /// volumen las repone, que en un enlace parado es nunca. Medido en la
    /// malla mTLS 2026-08-31 (brazo QKD): el KME local llena antes de que el
    /// handshake instale la época → 2 NOTIFY en claro rechazados → 256 claves
    /// ENC a un lado y 0 DEC al otro, indefinidamente. Tras `timeout` se
    /// sigue (con aviso): un enlace cuyo handshake no converge no debe
    /// bloquear el refill para siempre — quedará el síntoma histórico, no un
    /// cuelgue nuevo.
    pub async fn wait_send_root(&self, timeout: std::time::Duration) {
        let RootSource::PerEpoch { store, .. } = &self.src else {
            return;
        };
        let deadline = tokio::time::Instant::now() + timeout;
        while store.highest().is_none() {
            if tokio::time::Instant::now() >= deadline {
                warn!(
                    peer = self.peer_id,
                    timeout_s = timeout.as_secs(),
                    "qkc.frame_auth: sin época tras el timeout; los NOTIFY saldrán en claro \
                     (y un peer con raíz los descartará)"
                );
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// ¿Hay que firmar este kind?
    ///
    /// El NOTIFY siempre que haya raíz: decide qué `key_ID` pide el peer a su
    /// KME, y es el único plano del enlace que el propio QKD no protege. Los
    /// frames de datos, sólo si el modo lo pide — son el volumen, y hay que
    /// poder migrarlos por fases.
    fn seals(&self, kind: u8) -> bool {
        match kind {
            wire::FRAME_KEY_IDS_NOTIFY => true,
            _ => self.mode.signs(),
        }
    }

    /// Convierte el frame en su variante autenticada: calcula el tag sobre todo
    /// el frame, anexa `session ‖ counter ‖ tag` al payload y cambia el kind.
    ///
    /// No-op si el kind no tiene variante autenticada (ACK, LOCAL_*, handshake)
    /// o si la política no firma este kind.
    pub fn seal(&self, frame: &mut Frame) {
        let Some(auth_kind) = wire::auth_kind_for(frame.kind) else {
            return;
        };
        if !self.seals(frame.kind) {
            return;
        }
        // Sin material (PerEpoch antes de la primera época) no se sella; el
        // relay no habría llegado aquí porque tampoco hay OTP que cifrar.
        let Some((session, send_key)) = self.send_material() else {
            return;
        };
        let counter = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        let tag = {
            let aad = FrameAad {
                kind: auth_kind,
                grade: frame.grade,
                session,
                counter,
                sender_id: frame.sender_id,
                receiver_id: frame.receiver_id,
                dest_final: frame.dest_final,
                key_size_bits: frame.key_size_bits,
                epoch_id: frame.epoch_id,
                key_ids: &frame.key_ids,
                header_orr_mp: &frame.header_orr_mp,
                header_dkms_mp: &frame.header_dkms_mp,
                body: &frame.payload,
            };
            frame_mac::tag(&send_key, &aad)
        };
        wire::append_auth_trailer(&mut frame.payload, session, counter, &tag);
        frame.kind = auth_kind;
        self.stats.signed.fetch_add(1, Ordering::Relaxed);
    }

    /// Clave de la sesión del peer, cacheada. `recv` ya está bloqueado.
    /// `None` en `PerEpoch` si aún no tenemos el secreto de esa época (el
    /// emisor rotó y su frame llegó antes de que el handshake nos instalase la
    /// época): el frame se descarta como si el MAC no cuadrara.
    fn peer_key(&self, st: &mut RecvState, session: u64) -> Option<Zeroizing<[u8; 32]>> {
        if let Some((s, k)) = &st.cached {
            if *s == session {
                return Some(k.clone());
            }
        }
        let k = match &self.src {
            RootSource::Fixed { root, .. } => Zeroizing::new(frame_mac::derive_key(root, session)),
            RootSource::PerEpoch { store, .. } => {
                let secret = store.get(session as u32)?;
                Zeroizing::new(frame_mac::derive_key(secret.as_ref(), session))
            }
        };
        st.cached = Some((session, k.clone()));
        Some(k)
    }

    /// Verifica un frame autenticado, comprueba la frescura y lo deja como el
    /// frame base (kind sin `_AUTH`, payload sin trailer) para que el resto del
    /// QKC no se entere de nada.
    pub fn open(&self, frame: &mut Frame) -> Result<(), FrameAuthError> {
        let (session, counter) = {
            let (body, session, counter, tag) =
                wire::split_auth_trailer(&frame.payload).map_err(|_| FrameAuthError::Truncated)?;
            let mut st = self.recv.lock();
            let Some(key) = self.peer_key(&mut st, session) else {
                // PerEpoch sin el secreto de esa época todavía: descartar como
                // un MAC que no cuadra (el emisor lo regenerará; el lookahead
                // del handshake hace que esta ventana sea rara y breve).
                self.stats.bad_mac.fetch_add(1, Ordering::Relaxed);
                return Err(FrameAuthError::BadMac);
            };
            let aad = FrameAad {
                kind: frame.kind,
                grade: frame.grade,
                session,
                counter,
                sender_id: frame.sender_id,
                receiver_id: frame.receiver_id,
                dest_final: frame.dest_final,
                key_size_bits: frame.key_size_bits,
                epoch_id: frame.epoch_id,
                key_ids: &frame.key_ids,
                header_orr_mp: &frame.header_orr_mp,
                header_dkms_mp: &frame.header_dkms_mp,
                body,
            };
            if frame_mac::verify(&key, &aad, tag).is_err() {
                self.stats.bad_mac.fetch_add(1, Ordering::Relaxed);
                return Err(FrameAuthError::BadMac);
            }
            // MAC válido: sólo ahora se le deja tocar la ventana.
            if let Err(e) = st.window.check_and_set(session, counter) {
                self.stats.replayed.fetch_add(1, Ordering::Relaxed);
                return Err(FrameAuthError::Replay(e));
            }
            (session, counter)
        };
        let cut = frame.payload.len() - AUTH_TRAILER_LEN;
        frame.payload.truncate(cut);
        frame.kind = wire::base_kind_of(frame.kind);
        self.stats.verified.fetch_add(1, Ordering::Relaxed);
        tracing::trace!(
            peer = self.peer_id,
            session,
            counter,
            "qkc.frame_auth.verified"
        );
        Ok(())
    }

    /// Decide qué hacer con un frame que ha llegado **sin** trailer en un enlace
    /// con raíz. El NOTIFY se descarta siempre (fail-closed: teniendo con qué
    /// comprobarlo, aceptarlo sin comprobar no tiene sentido). Los frames de
    /// datos, según el modo: `require` descarta, `prefer` acepta y cuenta —
    /// que es lo que hace posible migrar sin cortar el tráfico.
    pub fn accept_plaintext(&self, kind: u8) -> Result<(), FrameAuthError> {
        let reject = match kind {
            wire::FRAME_KEY_IDS_NOTIFY => true,
            _ => self.mode.rejects_plaintext(),
        };
        if reject {
            // Con la PSK puesta solo en un extremo esto es CADA frame: el
            // contador (`plain_rej` en la línea `qkc.frame_auth`) lleva la
            // escala; el log habla en las potencias de dos.
            let n = self
                .stats
                .plaintext_rejected
                .fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    peer = self.peer_id,
                    kind,
                    rejected = n + 1,
                    "qkc.frame_auth: frame sin MAC en un enlace autenticado; descarto"
                );
            }
            return Err(FrameAuthError::PlaintextRejected);
        }
        self.stats
            .plaintext_accepted
            .fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Punto de entrada único para todo lo que llega del peer.
///
/// Las cuatro combinaciones importan, y las dos partes del QKC que reciben
/// frames del enlace (`relay` para los datos, `peer_server` para el NOTIFY)
/// tienen que tratarlas igual:
///
/// * enlace con raíz + frame autenticado → se verifica MAC y frescura.
/// * enlace con raíz + frame en claro → lo decide [`LinkFrameAuth::accept_plaintext`].
/// * enlace sin raíz + frame autenticado → no hay con qué comprobarlo. Se le
///   quita el trailer y se avisa: si no, quien lo consuma después contaría
///   [`AUTH_TRAILER_LEN`] bytes de más y fallaría con un error que no dice nada
///   del motivo real.
/// * enlace sin raíz + frame en claro → comportamiento histórico.
pub fn authenticate(
    fa: Option<&Arc<LinkFrameAuth>>,
    frame: &mut Frame,
) -> Result<(), FrameAuthError> {
    match (fa, wire::is_auth_kind(frame.kind)) {
        (Some(fa), true) => fa.open(frame),
        (Some(fa), false) => fa.accept_plaintext(frame.kind),
        (None, true) => {
            // La config asimétrica inversa: también es cada frame.
            static UNCHECKED: AtomicU64 = AtomicU64::new(0);
            let n = UNCHECKED.fetch_add(1, Ordering::Relaxed);
            if common::log_throttle::nth_is_loud(n) {
                warn!(
                    sender = frame.sender_id,
                    kind = frame.kind,
                    unchecked = n + 1,
                    "qkc.frame_auth: frame autenticado en un enlace sin link_psk; \
                     no se puede comprobar (config asimétrica)"
                );
            }
            let cut = frame
                .payload
                .len()
                .checked_sub(AUTH_TRAILER_LEN)
                .ok_or(FrameAuthError::Truncated)?;
            frame.payload.truncate(cut);
            frame.kind = wire::base_kind_of(frame.kind);
            Ok(())
        }
        (None, false) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    /// `wait_send_root`: con raíz per-época, espera a la primera época (el
    /// primer NOTIFY nunca sale en claro por la carrera KME-vs-handshake) y
    /// es no-op con raíz fija.
    #[tokio::test]
    async fn wait_send_root_waits_for_the_first_epoch() {
        use std::time::Duration;
        let store = crate::pqc_source::SecretStore::new(2, 0);
        let fa = std::sync::Arc::new(LinkFrameAuth::per_epoch(
            crate::config::FrameAuth::Require,
            2,
            std::sync::Arc::clone(&store),
        ));
        let waiter = tokio::spawn({
            let fa = std::sync::Arc::clone(&fa);
            async move { fa.wait_send_root(Duration::from_secs(5)).await }
        });
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!waiter.is_finished(), "sin época debe seguir esperando");
        store.insert(0, [7u8; 32]);
        tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("desbloquea al llegar la época")
            .unwrap();
        // Raíz fija: inmediato.
        let fixed =
            LinkFrameAuth::new(crate::config::FrameAuth::Prefer, 2, Some(vec![9u8; 32])).unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            fixed.wait_send_root(Duration::from_secs(5)),
        )
        .await
        .expect("Fixed no espera");
    }

    use super::*;
    use wire::{FRAME_RECV, FRAME_RECV_AUTH, FRAME_RELAY, FRAME_RELAY_AUTH};

    const ROOT: &[u8] = b"shared link psk for both ends!!!";

    fn mk(mode: FrameAuth) -> LinkFrameAuth {
        LinkFrameAuth::new(mode, 2, Some(ROOT.to_vec())).unwrap()
    }

    /// Raíz per-época: sella con el secreto de la época del enlace (el que el
    /// handshake ya negocia), sin `link_psk`. Round-trip, rotación y el frame
    /// cuya época el receptor aún no tiene.
    #[test]
    fn per_epoch_seals_with_the_link_secret_and_rotates() {
        use crate::pqc_source::SecretStore;
        // Dos stores (los dos extremos) con el MISMO secreto por época, que es
        // lo que el handshake ML-KEM garantiza.
        let a = SecretStore::new(2, 0);
        let b = SecretStore::new(2, 0);
        a.insert(5, [0x11; 32]);
        b.insert(5, [0x11; 32]);
        let tx = LinkFrameAuth::per_epoch(FrameAuth::Require, 2, a.clone());
        let rx = LinkFrameAuth::per_epoch(FrameAuth::Require, 2, b.clone());

        // Round-trip con la época 5.
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        assert_eq!(f.kind, FRAME_RECV_AUTH, "se selló con el secreto de época");
        rx.open(&mut f).unwrap();
        assert_eq!(f.kind, FRAME_RECV);

        // Rotación: ambos instalan la época 6; el emisor sella con la nueva
        // (highest) y el receptor la sigue.
        a.insert(6, [0x22; 32]);
        b.insert(6, [0x22; 32]);
        let mut f2 = frame(FRAME_RECV);
        tx.seal(&mut f2);
        rx.open(&mut f2).unwrap();

        // Un frame cuya época el receptor aún no tiene se descarta (no se
        // acepta sin verificar).
        a.insert(7, [0x33; 32]); // solo el emisor
        let mut f3 = frame(FRAME_RECV);
        tx.seal(&mut f3);
        assert!(matches!(rx.open(&mut f3), Err(FrameAuthError::BadMac)));
    }

    fn frame(kind: u8) -> Frame {
        let mut f = Frame::empty(kind);
        f.sender_id = 1;
        f.receiver_id = 2;
        f.dest_final = 9;
        f.key_size_bits = 256;
        f.key_ids = vec![uuid::Uuid::nil().to_string()];
        f.header_orr_mp = b"orr".to_vec();
        f.header_dkms_mp = b"dkms".to_vec();
        f.payload = b"ciphertext-de-prueba".to_vec();
        f
    }

    /// Emisor y receptor son objetos distintos con la MISMA raíz — como los dos
    /// extremos reales del enlace.
    fn pair() -> (LinkFrameAuth, LinkFrameAuth) {
        (mk(FrameAuth::Require), mk(FrameAuth::Require))
    }

    #[test]
    fn seal_open_round_trip() {
        let (tx, rx) = pair();
        let original = frame(FRAME_RECV);
        let mut f = original.clone();
        tx.seal(&mut f);
        assert_eq!(f.kind, FRAME_RECV_AUTH);
        assert_eq!(f.payload.len(), original.payload.len() + AUTH_TRAILER_LEN);

        rx.open(&mut f).unwrap();
        // Tras abrir, el frame es indistinguible del original.
        assert_eq!(f.kind, FRAME_RECV);
        assert_eq!(f.payload, original.payload);
        assert_eq!(rx.stats.verified.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn relay_kind_maps_too() {
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RELAY);
        tx.seal(&mut f);
        assert_eq!(f.kind, FRAME_RELAY_AUTH);
        rx.open(&mut f).unwrap();
        assert_eq!(f.kind, FRAME_RELAY);
    }

    #[test]
    fn tampered_ciphertext_is_caught() {
        // ESTE es el agujero que el OTP deja abierto: XOR es maleable, así que
        // sin MAC el receptor descifraría un plaintext modificado sin notarlo.
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        f.payload[0] ^= 0xFF;
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::BadMac));
        assert_eq!(rx.stats.bad_mac.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn spoofed_sender_is_caught() {
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        f.sender_id = 77;
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::BadMac));
    }

    #[test]
    fn redirected_frame_is_caught() {
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RELAY);
        tx.seal(&mut f);
        f.dest_final = 4242;
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::BadMac));
    }

    #[test]
    fn tampered_headers_are_caught() {
        // El QKC propaga los headers ORR/DKMS byte a byte sin mirarlos; el MAC
        // es lo único que impide que alguien los reescriba en tránsito.
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RELAY);
        tx.seal(&mut f);
        f.header_dkms_mp = b"otro!".to_vec();
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::BadMac));
    }

    #[test]
    fn different_psk_does_not_verify() {
        let tx = mk(FrameAuth::Require);
        let rx = LinkFrameAuth::new(FrameAuth::Require, 2, Some(b"otra psk".to_vec())).unwrap();
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::BadMac));
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let (tx, rx) = pair();
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        let captured = f.clone();
        rx.open(&mut f).unwrap();
        // El atacante reinyecta el frame tal cual, byte a byte.
        let mut again = captured;
        assert!(matches!(
            rx.open(&mut again),
            Err(FrameAuthError::Replay(_))
        ));
        assert_eq!(rx.stats.replayed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn counters_are_monotonic_and_start_at_one() {
        let tx = mk(FrameAuth::Require);
        for expected in 1..=5u64 {
            let mut f = frame(FRAME_RECV);
            tx.seal(&mut f);
            let (_, session, counter, _) = wire::split_auth_trailer(&f.payload).unwrap();
            assert_eq!(session, tx.session());
            assert_eq!(counter, expected);
        }
    }

    #[test]
    fn out_of_order_delivery_still_works() {
        // El emisor numera desde varias tareas (max_emits_in_flight), así que
        // llegar desordenado dentro de la ventana es normal, no un ataque.
        let (tx, rx) = pair();
        let mut sealed: Vec<Frame> = (0..8)
            .map(|_| {
                let mut f = frame(FRAME_RECV);
                tx.seal(&mut f);
                f
            })
            .collect();
        sealed.reverse();
        for mut f in sealed {
            rx.open(&mut f).unwrap();
        }
        assert_eq!(rx.stats.verified.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn peer_restart_is_accepted_but_old_session_cannot_be_revived() {
        let rx = mk(FrameAuth::Require);
        let tx1 = mk(FrameAuth::Require);
        let mut f1 = frame(FRAME_RECV);
        tx1.seal(&mut f1);
        let captured = f1.clone();
        rx.open(&mut f1).unwrap();

        // El peer reinicia: sesión nueva, contadores desde 1 otra vez.
        let tx2 = mk(FrameAuth::Require);
        assert_ne!(tx1.session(), tx2.session());
        let mut f2 = frame(FRAME_RECV);
        tx2.seal(&mut f2);
        rx.open(&mut f2).unwrap();

        // Y el frame de la sesión vieja ya no vale, aunque su MAC sea correcto.
        let mut old = captured;
        assert!(matches!(rx.open(&mut old), Err(FrameAuthError::Replay(_))));
    }

    #[test]
    fn truncated_trailer_is_rejected() {
        let rx = mk(FrameAuth::Require);
        let mut f = frame(FRAME_RECV_AUTH);
        f.payload = vec![0u8; AUTH_TRAILER_LEN - 1];
        assert_eq!(rx.open(&mut f), Err(FrameAuthError::Truncated));
    }

    #[test]
    fn plaintext_policy_depends_on_mode() {
        assert!(mk(FrameAuth::Prefer).accept_plaintext(FRAME_RECV).is_ok());
        assert_eq!(
            mk(FrameAuth::Require).accept_plaintext(FRAME_RECV),
            Err(FrameAuthError::PlaintextRejected)
        );
    }

    #[test]
    fn notify_is_always_authenticated_when_there_is_a_root() {
        // El NOTIFY no sigue el modo: teniendo con qué comprobarlo, aceptarlo
        // sin comprobar no tiene sentido. Vale hasta en `off`.
        for mode in [FrameAuth::Off, FrameAuth::Prefer, FrameAuth::Require] {
            let fa = LinkFrameAuth::new(mode, 2, Some(ROOT.to_vec())).unwrap();
            let mut f = frame(wire::FRAME_KEY_IDS_NOTIFY);
            fa.seal(&mut f);
            assert_eq!(f.kind, wire::FRAME_KEY_IDS_NOTIFY_AUTH, "modo {mode:?}");
            assert_eq!(
                fa.accept_plaintext(wire::FRAME_KEY_IDS_NOTIFY),
                Err(FrameAuthError::PlaintextRejected),
                "modo {mode:?}"
            );
        }
    }

    #[test]
    fn notify_replay_is_rejected() {
        // El defecto que arregla esto: el HMAC anterior del NOTIFY iba con
        // `epoch = 0` y sin contador, así que un NOTIFY capturado verificaba
        // igual y hacía al peer volver a pedir esos `key_ID` a su KME.
        let (tx, rx) = pair();
        let mut f = frame(wire::FRAME_KEY_IDS_NOTIFY);
        tx.seal(&mut f);
        let captured = f.clone();
        rx.open(&mut f).unwrap();
        assert_eq!(f.kind, wire::FRAME_KEY_IDS_NOTIFY);
        let mut again = captured;
        assert!(matches!(
            rx.open(&mut again),
            Err(FrameAuthError::Replay(_))
        ));
    }

    #[test]
    fn data_frames_follow_the_mode_but_notify_does_not() {
        // En `off` con raíz: el NOTIFY se sella, los datos no.
        let fa = LinkFrameAuth::new(FrameAuth::Off, 2, Some(ROOT.to_vec())).unwrap();
        let mut data = frame(FRAME_RECV);
        fa.seal(&mut data);
        assert_eq!(data.kind, FRAME_RECV, "en off los datos van sin MAC");
        let mut notify = frame(wire::FRAME_KEY_IDS_NOTIFY);
        fa.seal(&mut notify);
        assert_eq!(notify.kind, wire::FRAME_KEY_IDS_NOTIFY_AUTH);
    }

    #[test]
    fn without_a_root_there_is_nothing_to_build() {
        assert!(LinkFrameAuth::new(FrameAuth::Prefer, 2, None).is_none());
        assert!(LinkFrameAuth::new(FrameAuth::Off, 2, None).is_none());
        // Con raíz sí, en cualquier modo: el NOTIFY la necesita.
        assert!(LinkFrameAuth::new(FrameAuth::Off, 2, Some(ROOT.to_vec())).is_some());
    }

    #[test]
    fn authenticate_strips_the_trailer_when_there_is_no_root() {
        // Config asimétrica: el peer sella y nosotros no tenemos PSK. Hay que
        // quitar el trailer igualmente, o `decrypt` contaría 48 B de más.
        let tx = mk(FrameAuth::Require);
        let original = frame(FRAME_RECV);
        let mut f = original.clone();
        tx.seal(&mut f);
        authenticate(None, &mut f).unwrap();
        assert_eq!(f.kind, FRAME_RECV);
        assert_eq!(f.payload, original.payload);
    }

    #[test]
    fn authenticate_routes_all_four_cases() {
        let tx = mk(FrameAuth::Require);
        let rx = mk(FrameAuth::Require);
        let rx_arc = Arc::new(mk(FrameAuth::Prefer));

        // con raíz + autenticado → verifica
        let mut f = frame(FRAME_RECV);
        tx.seal(&mut f);
        assert!(authenticate(Some(&Arc::new(rx)), &mut f).is_ok());

        // con raíz + en claro, prefer → pasa
        let mut f = frame(FRAME_RECV);
        assert!(authenticate(Some(&rx_arc), &mut f).is_ok());

        // con raíz + en claro, require → se descarta
        let mut f = frame(FRAME_RECV);
        assert_eq!(
            authenticate(Some(&Arc::new(mk(FrameAuth::Require))), &mut f),
            Err(FrameAuthError::PlaintextRejected)
        );

        // sin raíz + en claro → comportamiento histórico
        let mut f = frame(FRAME_RECV);
        assert!(authenticate(None, &mut f).is_ok());
    }

    #[test]
    fn seal_is_a_noop_for_kinds_without_auth_variant() {
        let tx = mk(FrameAuth::Require);
        let mut f = frame(wire::FRAME_ACK);
        let before = f.payload.clone();
        tx.seal(&mut f);
        assert_eq!(f.kind, wire::FRAME_ACK);
        assert_eq!(f.payload, before);
    }
}
