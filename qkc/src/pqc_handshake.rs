//! Handshake ML-KEM de un enlace PQC, sobre el canal TCP QKC↩QKC.
//!
//! Establece el secreto compartido de 32 B del que [`crate::pqc_source`]
//! deriva el flujo de claves. Reutiliza `common::crypto::pqc` (ML-KEM); NO
//! depende de `orr`.
//!
//! Roles deterministas por id (igual criterio que el lex-smaller de ORR):
//! el QKC con `qkc_id` **menor** es el iniciador, el mayor el respondedor.
//! Exactamente uno inicia.
//!
//! ```text
//! Iniciador (id menor)                 Respondedor (id mayor)
//!   keygen() → (pk, sk)
//!   guarda sk
//!   ── FRAME_PQC_KEM_INIT{pk} ───────►
//!                                       encap(pk) → (ct, ss)
//!                                       guarda ss, cachea ct
//!   ◄───── FRAME_PQC_KEM_RESP{ct} ──
//!   decap(sk, ct) → ss
//!   guarda ss
//! ```
//!
//! Robustez: el iniciador **reenvía el mismo INIT** (misma pk/sk) con
//! backoff hasta que el secreto está (cubre que `peer_out` dropea frames
//! mientras el writer reconecta). El respondedor **cachea su ciphertext** y
//! lo reenvía ante INITs duplicados — re-encapsular daría otro secreto.
//! Los frames de un peer se procesan secuencialmente en el read-loop de
//! `peer_server`, así que no hay encap/decap concurrentes para un enlace.

use std::{sync::Arc, time::Duration};

use parking_lot::Mutex;
use tokio::sync::watch;
use tracing::{debug, info, warn};
use wire::{Frame, FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_RESP};
use zeroize::Zeroizing;

use crate::{pqc_source::SecretWatch, transport::peer_client::PeerOut};

/// Reintento del INIT mientras el handshake no completa.
const INIT_RETRY: Duration = Duration::from_millis(300);

type SecretSender = watch::Sender<Option<Zeroizing<[u8; 32]>>>;

/// Coordinador del handshake ML-KEM de UN enlace PQC.
pub struct PqcHandshake {
    suite: String,
    my_id: u32,
    peer_id: u32,
    peer_addr: String,
    peer_out: Arc<PeerOut>,
    secret_tx: SecretSender,
    /// Decap key del iniciador, viva hasta que llega el RESP.
    pending_sk: Mutex<Option<Zeroizing<Vec<u8>>>>,
    /// Ciphertext cacheado del respondedor (idempotencia ante INIT dup).
    resp_cache: Mutex<Option<Vec<u8>>>,
}

impl PqcHandshake {
    /// Crea el coordinador y devuelve el [`SecretWatch`] que consume el
    /// [`crate::pqc_source::PqcKeySource`] del mismo enlace.
    pub fn new(
        suite: String,
        my_id: u32,
        peer_id: u32,
        peer_addr: String,
        peer_out: Arc<PeerOut>,
    ) -> (Arc<Self>, SecretWatch) {
        let (secret_tx, secret_rx) = watch::channel::<Option<Zeroizing<[u8; 32]>>>(None);
        let hs = Arc::new(Self {
            suite,
            my_id,
            peer_id,
            peer_addr,
            peer_out,
            secret_tx,
            pending_sk: Mutex::new(None),
            resp_cache: Mutex::new(None),
        });
        (hs, secret_rx)
    }

    fn is_initiator(&self) -> bool {
        self.my_id < self.peer_id
    }

    fn secret_ready(&self) -> bool {
        self.secret_tx.borrow().is_some()
    }

    /// Publica el secreto (solo la primera vez; idempotente). Valida 32 B.
    fn publish_secret(&self, ss: Vec<u8>) {
        if ss.len() != 32 {
            warn!(
                peer = self.peer_id,
                len = ss.len(),
                "qkc.pqc: shared secret not 32 B"
            );
            return;
        }
        if self.secret_tx.borrow().is_some() {
            return;
        }
        let mut arr = [0u8; 32];
        arr.copy_from_slice(&ss);
        let _ = self.secret_tx.send(Some(Zeroizing::new(arr)));
        info!(
            me = self.my_id,
            peer = self.peer_id,
            "qkc.pqc.handshake.established"
        );
    }

    fn send(&self, kind: u8, payload: Vec<u8>) {
        let mut f = Frame::empty(kind);
        f.sender_id = self.my_id;
        f.receiver_id = self.peer_id;
        f.dest_final = self.peer_id;
        f.payload = payload;
        self.peer_out.send(self.peer_id, &self.peer_addr, f);
    }

    /// Lanza la tarea iniciadora (no-op si somos el respondedor). Genera el
    /// keypair UNA vez y reenvía la misma pubkey hasta que el secreto esté.
    pub fn spawn_initiator(self: &Arc<Self>) {
        if !self.is_initiator() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            let kem = match common::crypto::pqc::kem_for(&me.suite) {
                Ok(k) => k,
                Err(e) => {
                    warn!(peer = me.peer_id, error = %e, "qkc.pqc: kem_for failed");
                    return;
                }
            };
            let kp = match kem.keygen() {
                Ok(k) => k,
                Err(e) => {
                    warn!(peer = me.peer_id, error = %e, "qkc.pqc: keygen failed");
                    return;
                }
            };
            *me.pending_sk.lock() = Some(Zeroizing::new(kp.secret));
            let pubkey = kp.public;
            let mut attempts: u64 = 0;
            while !me.secret_ready() {
                me.send(FRAME_PQC_KEM_INIT, pubkey.clone());
                attempts += 1;
                if attempts % 20 == 0 {
                    debug!(
                        peer = me.peer_id,
                        attempts, "qkc.pqc.handshake.init_retrying"
                    );
                }
                tokio::time::sleep(INIT_RETRY).await;
            }
            // Secreto establecido: libera la decap key.
            *me.pending_sk.lock() = None;
        });
    }

    /// Respondedor: llegó un INIT con la pubkey del peer.
    pub fn handle_init(&self, peer_pubkey: &[u8]) {
        // Idempotencia: si ya encapsulamos, reenvía el ciphertext cacheado.
        if let Some(ct) = self.resp_cache.lock().clone() {
            self.send(FRAME_PQC_KEM_RESP, ct);
            return;
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
        self.publish_secret(encap.shared_secret);
        *self.resp_cache.lock() = Some(encap.ciphertext.clone());
        self.send(FRAME_PQC_KEM_RESP, encap.ciphertext);
    }

    /// Iniciador: llegó el RESP con el ciphertext. Decapsula y publica.
    pub fn handle_resp(&self, ciphertext: &[u8]) {
        if self.secret_ready() {
            return;
        }
        let sk = match self.pending_sk.lock().clone() {
            Some(sk) => sk,
            None => {
                warn!(peer = self.peer_id, "qkc.pqc: RESP without pending sk");
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
            Ok(ss) => self.publish_secret(ss),
            Err(e) => warn!(peer = self.peer_id, error = %e, "qkc.pqc: decap failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handshake(my_id: u32, peer_id: u32) -> (Arc<PqcHandshake>, SecretWatch) {
        PqcHandshake::new(
            common::crypto::pqc::suite::ML_KEM_768.to_string(),
            my_id,
            peer_id,
            "127.0.0.1:1".to_string(),
            Arc::new(PeerOut::new()),
        )
    }

    /// Round-trip in-process: el respondedor encapsula sobre la pubkey del
    /// iniciador y el iniciador decapsula el ciphertext; ambos quedan con
    /// el MISMO secreto de 32 B. (No usamos el socket: invocamos los
    /// handlers directamente con los blobs que viajarían en `payload`.)
    // `#[tokio::test]`: handle_init/handle_resp llaman a `send`, que en el
    // primer envío hace `tokio::spawn` del writer del PeerOut → necesita un
    // runtime. El writer intenta conectar a 127.0.0.1:1 y falla en
    // background, irrelevante para lo que validamos (la derivación).
    #[tokio::test]
    async fn handshake_round_trip_yields_equal_32b_secret() {
        // Iniciador (id menor) genera su keypair manualmente para extraer
        // la pubkey que iría en el INIT y dejar la sk en pending_sk.
        let (ini, ini_rx) = handshake(1, 2);
        let (resp, resp_rx) = handshake(2, 1);

        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        *ini.pending_sk.lock() = Some(Zeroizing::new(kp.secret.clone()));

        // Respondedor recibe el INIT (pubkey) → encapsula y cachea ct.
        resp.handle_init(&kp.public);
        let ct = resp
            .resp_cache
            .lock()
            .clone()
            .expect("responder cached ciphertext");

        // Iniciador recibe el RESP (ct) → decapsula.
        ini.handle_resp(&ct);

        let s_ini = ini_rx.borrow().clone().expect("initiator secret set");
        let s_resp = resp_rx.borrow().clone().expect("responder secret set");
        assert_eq!(*s_ini, *s_resp, "both ends must share the same 32 B secret");
        assert_eq!(s_ini.len(), 32);
    }

    /// INIT duplicado: el respondedor reenvía el ciphertext cacheado sin
    /// re-encapsular (el secreto no cambia).
    #[tokio::test]
    async fn duplicate_init_keeps_same_secret() {
        let (resp, resp_rx) = handshake(2, 1);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();

        resp.handle_init(&kp.public);
        let s1 = resp_rx.borrow().clone().unwrap();
        let ct1 = resp.resp_cache.lock().clone().unwrap();

        resp.handle_init(&kp.public); // duplicado
        let s2 = resp_rx.borrow().clone().unwrap();
        let ct2 = resp.resp_cache.lock().clone().unwrap();

        assert_eq!(*s1, *s2, "secret must not change on duplicate INIT");
        assert_eq!(ct1, ct2, "cached ciphertext must be stable");
    }

    #[test]
    fn role_split_is_deterministic() {
        let (a, _) = handshake(1, 2);
        let (b, _) = handshake(2, 1);
        assert!(a.is_initiator());
        assert!(!b.is_initiator());
    }
}
