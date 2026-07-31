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
use std::{sync::Arc, time::Duration};

use parking_lot::Mutex;
use tracing::{debug, info, warn};
use wire::{Frame, FRAME_PQC_KEM_INIT, FRAME_PQC_KEM_RESP};
use zeroize::Zeroizing;

use crate::{
    pqc_source::{RekeyClock, SecretStore},
    transport::peer_client::PeerOut,
};

/// Reintento del INIT mientras una época no completa.
const INIT_RETRY: Duration = Duration::from_millis(300);

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
        })
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

    fn send(&self, kind: u8, epoch: u32, blob: &[u8]) {
        let mut payload = Vec::with_capacity(4 + blob.len());
        payload.extend_from_slice(&epoch.to_be_bytes());
        payload.extend_from_slice(blob);
        let mut f = Frame::empty(kind);
        f.sender_id = self.my_id;
        f.receiver_id = self.peer_id;
        f.dest_final = self.peer_id;
        f.payload = payload;
        self.peer_out.send(self.peer_id, &self.peer_addr, f);
    }

    /// Arranca la tarea de rotación/pre-carga. No-op en el respondedor (que es
    /// reactivo: encapsula al recibir cada INIT). El iniciador establece las
    /// épocas `0..=lookahead` y luego añade una por cada disparo de rotación.
    pub fn spawn_rotation(self: &Arc<Self>) {
        if !self.is_initiator() {
            return;
        }
        let me = self.clone();
        tokio::spawn(async move {
            for epoch in 0..=me.lookahead {
                me.establish(epoch).await;
            }
            // Sin disparadores ⇒ una sola época (comportamiento histórico).
            if me.clock_disabled() {
                return;
            }
            loop {
                me.clock.wait_rotate(me.rekey_secs).await;
                let next = me.store.highest().unwrap_or(0).saturating_add(1);
                me.establish(next).await;
            }
        });
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
            self.send(FRAME_PQC_KEM_INIT, epoch, &pubkey);
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

    /// Respondedor: llegó un INIT `época‖pubkey`.
    pub fn handle_init(&self, payload: &[u8]) {
        let Some((epoch, peer_pubkey)) = split_epoch(payload) else {
            warn!(peer = self.peer_id, "qkc.pqc: INIT payload too short");
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
                self.send(FRAME_PQC_KEM_RESP, epoch, &ct);
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
        self.send(FRAME_PQC_KEM_RESP, epoch, &encap.ciphertext);
    }

    /// Iniciador: llegó el RESP `época‖ciphertext`. Decapsula y publica.
    pub fn handle_resp(&self, payload: &[u8]) {
        let Some((epoch, ciphertext)) = split_epoch(payload) else {
            warn!(peer = self.peer_id, "qkc.pqc: RESP payload too short");
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
            resp.handle_init(&init_payload);
            let ct = resp.resp_cache.lock().get(&epoch).cloned().unwrap().1;

            // RESP = época ‖ ciphertext
            let mut resp_payload = epoch.to_be_bytes().to_vec();
            resp_payload.extend_from_slice(&ct);
            ini.handle_resp(&resp_payload);

            let s_ini = ini.store.get(epoch).unwrap();
            let s_resp = resp.store.get(epoch).unwrap();
            assert_eq!(
                *s_ini, *s_resp,
                "both ends share the same secret for the epoch"
            );
        }
    }

    #[tokio::test]
    async fn duplicate_init_keeps_same_secret_per_epoch() {
        let resp = handshake(2, 1);
        let kem = common::crypto::pqc::kem_for(common::crypto::pqc::suite::ML_KEM_768).unwrap();
        let kp = kem.keygen().unwrap();
        let mut init = 3u32.to_be_bytes().to_vec();
        init.extend_from_slice(&kp.public);

        resp.handle_init(&init);
        let s1 = resp.store.get(3).unwrap();
        let ct1 = resp.resp_cache.lock().get(&3).cloned().unwrap().1;
        resp.handle_init(&init); // duplicado
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
        resp.handle_init(&init_old);
        let s_old = resp.store.get(epoch).unwrap();

        // Se reinicia: keypair nuevo, misma época.
        let ini = handshake(1, 2);
        let kp_new = kem.keygen().unwrap();
        ini.pending_sk
            .lock()
            .insert(epoch, Zeroizing::new(kp_new.secret.clone()));
        let mut init_new = epoch.to_be_bytes().to_vec();
        init_new.extend_from_slice(&kp_new.public);
        resp.handle_init(&init_new);

        let (cached_pk, ct_new) = resp.resp_cache.lock().get(&epoch).cloned().unwrap();
        assert_eq!(cached_pk, kp_new.public, "la caché sigue a la pubkey nueva");

        let mut resp_payload = epoch.to_be_bytes().to_vec();
        resp_payload.extend_from_slice(&ct_new);
        ini.handle_resp(&resp_payload);

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
        );
        assert!(hs.clock_disabled());
        // (epoch_of sanity, para no dejar el import sin uso si se recorta arriba)
        assert_eq!(
            epoch_of(&[0, 0, 0, 5, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]),
            5
        );
    }
}
