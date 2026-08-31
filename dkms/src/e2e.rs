//! Capa extremo a extremo DKMS↔DKMS sobre cada `DKMS_BUFFER`.
//!
//! Hasta 2026-08-28 el extremo criptográfico del material de transporte
//! estaba un salto antes de donde vive la relación: la cebolla del ORR
//! (`max_hops = 1`) sellaba ORR_A↔ORR_B, y ORR_B entregaba a su DKMS la clave
//! en claro. En una máquina da igual; con un módulo por institución, el ORR
//! destino leía el material que DKMS_A le mandaba a DKMS_B. Y todo el
//! bootstrap ORR↔ORR —con sus dos bugs abiertos, el re-bootstrap pasivo y el
//! encap concurrente— estaba en el camino crítico de cada clave.
//!
//! Ahora sella el **DKMS**, que es el dueño de la relación, y el ORR queda en
//! lo que su nombre dice: relay. `max_hops = 0` por defecto; los modos onion
//! siguen ahí como privacidad de camino opcional y envuelven un payload que ya
//! va sellado.
//!
//! ## Acuerdo de clave
//!
//! Los dos DKMS ya tienen un canal autenticado en los dos sentidos: el mTLS
//! ETSI-020 con certificados ML-DSA (`peer_client`, y `DkmsPeer` en el
//! servidor saca la identidad del certificado). Sobre él, `POST
//! /kmapi/v1/e2e/kem`: quien pide manda una pública ML-KEM **efímera**; quien
//! responde encapsula, **asigna el número de época** y devuelve el ct. Los dos
//! guardan `master[epoch]` y pasan a emitir con esa época.
//!
//! Dos decisiones que evitan por construcción los fallos del ORR:
//!
//! - **La época viaja en la cabecera de cada clave y la asigna el que
//!   responde, al azar.** Dos acuerdos concurrentes dan dos épocas distintas,
//!   nunca una época con dos secretos — que es la clase de fallo del encap
//!   concurrente (2026-08-02). Por eso no hace falta un iniciador único.
//! - **Pide el que no tiene clave.** El canal ya autentica a ambos, así que
//!   cualquiera puede pedir. Un DKMS que reinicia arranca sin épocas y es el
//!   primero en emitir (la misma idea que la detección por `incarnation`):
//!   pide, y como el que responde también pasa a emitir con la época nueva,
//!   los dos sentidos sanan sin que nadie tenga que "re-bootstrapear" al otro.
//!   Un receptor que ve una época que no tiene, pide también.
//!
//! La rotación por tiempo (`rekey_secs`) la dispara sólo el lex-menor del
//! par, para no rotar por duplicado. Cada acuerdo usa una keypair efímera que
//! se destruye al descapsular: forward secrecy por época.
//!
//! ## Por clave
//!
//! ```text
//! K ‖ nonce = HKDF-SHA256(salt = "dkms.e2e.v1", ikm = master[epoch],
//!                          info = key_id ‖ "dkms.e2e.v1", L = 44)
//! ct, tag   = AES-256-GCM(K, nonce, bytes, aad)
//! ```
//!
//! **El tag y la época van en `header_dkms`, no en el payload.** El OTP del
//! enlace QKC trocea el payload en bloques de `key_size_bits / 8` y gasta una
//! clave QKD por bloque: 16 bytes de tag convertirían un mensaje de 32 B en dos
//! bloques, el doble de material por salto (medido −51 % cuando la cebolla del
//! ORR lo hizo, 2026-08-28). La cabecera la propagan ORR y QKC byte a byte y no
//! cuesta material. El nonce se deriva del `key_id` en vez de viajar: cada K se
//! usa con exactamente un nonce, así que la reutilización es imposible.
//!
//! El AAD cubre la cabecera DKMS entera (menos el propio tag) más origen y
//! destino. Importa porque la cabecera lleva `incarnation`, y el receptor borra
//! sus buffers con el peer cuando la ve cambiar: sin autenticarla, cualquiera
//! que tocase un frame en un QKC de tránsito podía inventar una y borrar el
//! material de un par. Por eso el receptor **abre antes de actuar** sobre
//! nada de la cabecera.
//!
//! ## Frescura
//!
//! `(incarnation, e2e_ctr)`, ambos en el AAD. El contador es por par —el DKMS
//! es extremo real, no hay entrelazado multi-salto como en el ORR— así que la
//! secuencia es densa y basta la ventana por defecto. Sin esto, con passthrough
//! un QKC de tránsito reinyecta un `DKMS_BUFFER` y resucita en `buffer_dec`
//! una clave ya consumida: OTP reutilizado.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use hkdf::Hkdf;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tracing::{debug, info, warn};
use zeroize::Zeroizing;

use common::crypto::{
    aead,
    frame_mac::{ReplayError, ReplayWindow},
    pqc,
};

use crate::{config::TransportE2eCfg, peer_client::PeerHttpClient, peers::PeerRegistry};

/// Época del secreto con el que va sellado el payload (decimal).
pub const HDR_E2E_EPOCH: &str = "e2e_epoch";
/// Contador monotónico del emisor hacia este peer (decimal, empieza en 1).
pub const HDR_E2E_CTR: &str = "e2e_ctr";
/// Tag AES-GCM del payload (hex, 16 B).
pub const HDR_E2E_TAG: &str = "e2e_tag";
/// Ruta del acuerdo de clave en el plano peer (mTLS).
pub const KEM_PATH: &str = "/kmapi/v1/e2e/kem";

const HKDF_SALT: &[u8] = b"dkms.e2e.v1";
const HKDF_INFO: &[u8] = b"dkms.e2e.v1";
const AAD_DOMAIN: &[u8] = b"dkms.e2e.v1";
/// Entre dos intentos de acuerdo con el mismo peer. Un receptor que ve una
/// época desconocida lo dispara por cada frame que llega, y llegan cientos por
/// segundo.
const AGREEMENT_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// Una esk pendiente más vieja que esto es de un acuerdo que murió a medias.
const PENDING_ESK_TTL: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum E2eError {
    /// Nunca se ha acordado nada con este peer (o se perdió al reiniciar).
    #[error("sin época acordada con {peer}")]
    NoEpoch { peer: String },
    /// El emisor selló con una época que no tenemos: reiniciamos, o está más
    /// atrás que el histórico que guardamos.
    #[error("época {epoch} desconocida para {peer}")]
    UnknownEpoch { peer: String, epoch: u32 },
    #[error("cabecera e2e incompleta o malformada: {0}")]
    Header(String),
    /// Material alterado en tránsito, cabecera alterada, o secreto divergente.
    #[error("tag e2e inválido (material o cabecera alterados, o secreto divergente)")]
    BadTag,
    #[error("replay: {0}")]
    Replay(#[from] ReplayError),
    #[error("kem: {0}")]
    Kem(String),
    #[error("peer: {0}")]
    Peer(String),
}

/// Cuerpo de `POST /kmapi/v1/e2e/kem`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KemRequest {
    pub suite: String,
    /// Pública ML-KEM efímera, base64 estándar.
    pub epk: String,
}

/// Respuesta de `POST /kmapi/v1/e2e/kem`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KemResponse {
    pub suite: String,
    /// Época asignada por el que responde. Nunca 0.
    pub epoch: u32,
    /// Ciphertext ML-KEM, base64 estándar.
    pub ct: String,
}

struct PeerState {
    /// Secretos por época, en orden de establecimiento (el más nuevo al
    /// final). Se guardan los últimos `epoch_history_keep`.
    epochs: VecDeque<(u32, Zeroizing<[u8; 32]>)>,
    /// Época con la que sellamos lo que emitimos a este peer.
    send_epoch: Option<u32>,
    /// Contador de lo que le hemos emitido, por par.
    counter: u64,
    /// Ventana de lo que nos llega de él.
    replay: ReplayWindow,
    /// Secreta efímera del acuerdo que tenemos en vuelo, si lo pedimos
    /// nosotros.
    pending_esk: Option<(Zeroizing<Vec<u8>>, Instant)>,
    inflight: bool,
    last_attempt: Option<Instant>,
    established_at: Option<Instant>,
}

impl PeerState {
    fn new(window: u64) -> Self {
        Self {
            epochs: VecDeque::new(),
            send_epoch: None,
            counter: 0,
            replay: ReplayWindow::new(window),
            pending_esk: None,
            inflight: false,
            last_attempt: None,
            established_at: None,
        }
    }

    fn secret(&self, epoch: u32) -> Option<Zeroizing<[u8; 32]>> {
        self.epochs
            .iter()
            .find(|(e, _)| *e == epoch)
            .map(|(_, s)| s.clone())
    }

    fn store(&mut self, epoch: u32, ss: &[u8], keep: usize) -> Result<(), E2eError> {
        if ss.len() != 32 {
            return Err(E2eError::Kem(format!(
                "shared secret de {} bytes, se esperaban 32",
                ss.len()
            )));
        }
        let mut s = Zeroizing::new([0u8; 32]);
        s.copy_from_slice(ss);
        self.epochs.retain(|(e, _)| *e != epoch);
        self.epochs.push_back((epoch, s));
        while self.epochs.len() > keep.max(1) {
            self.epochs.pop_front();
        }
        self.send_epoch = Some(epoch);
        self.established_at = Some(Instant::now());
        Ok(())
    }

    fn fresh_epoch(&self) -> u32 {
        loop {
            let e = rand::random::<u32>();
            if e != 0 && !self.epochs.iter().any(|(x, _)| *x == e) {
                return e;
            }
        }
    }
}

/// Estado de la capa para todos los peers. Uno por proceso; lo comparten el
/// generador (sella), el pump de entregas (abre) y el servidor HTTP (responde
/// acuerdos).
pub struct E2e {
    my_id: String,
    cfg: TransportE2eCfg,
    peers: Mutex<HashMap<String, PeerState>>,
    client: Option<Arc<PeerHttpClient>>,
    registry: Arc<PeerRegistry>,
}

impl E2e {
    pub fn new(
        my_id: impl Into<String>,
        cfg: TransportE2eCfg,
        client: Option<Arc<PeerHttpClient>>,
        registry: Arc<PeerRegistry>,
    ) -> Self {
        Self {
            my_id: my_id.into(),
            cfg,
            peers: Mutex::new(HashMap::new()),
            client,
            registry,
        }
    }

    pub fn my_id(&self) -> &str {
        &self.my_id
    }

    fn state<'a>(&self, map: &'a mut HashMap<String, PeerState>, peer: &str) -> &'a mut PeerState {
        let window = self.cfg.replay_window;
        map.entry(peer.to_owned())
            .or_insert_with(|| PeerState::new(window))
    }

    // ─── derivación y AAD ───────────────────────────────────────────────

    fn derive(secret: &[u8; 32], key_id: &str) -> (Zeroizing<[u8; 32]>, [u8; aead::NONCE_LEN]) {
        let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), secret);
        let mut okm = Zeroizing::new([0u8; 32 + aead::NONCE_LEN]);
        hk.expand_multi_info(&[key_id.as_bytes(), HKDF_INFO], &mut *okm)
            .expect("44 bytes caben en HKDF-SHA256");
        let mut k = Zeroizing::new([0u8; 32]);
        k.copy_from_slice(&okm[..32]);
        let mut nonce = [0u8; aead::NONCE_LEN];
        nonce.copy_from_slice(&okm[32..]);
        (k, nonce)
    }

    /// AAD canónico: dominio, origen, destino y la cabecera DKMS ordenada por
    /// clave, sin el tag. Se calcula sobre pares `(k, v)` y no sobre la
    /// serialización, porque el emisor la manda como mapa proto y el receptor
    /// la recibe como otro mapa: lo único estable son las parejas.
    fn aad<'a>(from: &str, to: &str, pairs: impl Iterator<Item = (&'a str, &'a str)>) -> Vec<u8> {
        fn put(out: &mut Vec<u8>, s: &[u8]) {
            out.extend_from_slice(&(s.len() as u32).to_be_bytes());
            out.extend_from_slice(s);
        }
        let mut out = Vec::with_capacity(512);
        put(&mut out, AAD_DOMAIN);
        put(&mut out, from.as_bytes());
        put(&mut out, to.as_bytes());
        for (k, v) in pairs {
            if k == HDR_E2E_TAG {
                continue;
            }
            put(&mut out, k.as_bytes());
            put(&mut out, v.as_bytes());
        }
        out
    }

    // ─── sellar / abrir ─────────────────────────────────────────────────

    /// Sella `plaintext` para `peer`. Escribe `e2e_epoch`, `e2e_ctr` y
    /// `e2e_tag` en `header`; el resto de la cabecera tiene que estar ya
    /// completo, porque entra en el AAD. Devuelve el ciphertext, del mismo
    /// tamaño que el plaintext.
    pub fn seal(
        &self,
        peer: &str,
        key_id: &str,
        header: &mut BTreeMap<String, String>,
        plaintext: &[u8],
    ) -> Result<Vec<u8>, E2eError> {
        let (epoch, secret, counter) = {
            let mut g = self.peers.lock();
            let st = self.state(&mut g, peer);
            let epoch = st.send_epoch.ok_or_else(|| E2eError::NoEpoch {
                peer: peer.to_owned(),
            })?;
            let secret = st.secret(epoch).ok_or_else(|| E2eError::NoEpoch {
                peer: peer.to_owned(),
            })?;
            st.counter += 1;
            (epoch, secret, st.counter)
        };
        header.insert(HDR_E2E_EPOCH.into(), epoch.to_string());
        header.insert(HDR_E2E_CTR.into(), counter.to_string());
        header.remove(HDR_E2E_TAG);
        let aad = Self::aad(
            &self.my_id,
            peer,
            header.iter().map(|(k, v)| (k.as_str(), v.as_str())),
        );
        let (k, nonce) = Self::derive(&secret, key_id);
        let (ct, tag) = aead::seal_detached(&*k, &nonce, plaintext, &aad)
            .map_err(|e| E2eError::Kem(e.to_string()))?;
        header.insert(HDR_E2E_TAG.into(), hex::encode(tag));
        Ok(ct)
    }

    /// Abre un payload que dice venir de `source`. Verifica el tag **antes**
    /// de mirar la frescura, y no toca ningún estado hasta que cuadra.
    pub fn open(
        &self,
        source: &str,
        header: &HashMap<String, String>,
        key_id: &str,
        payload: &[u8],
    ) -> Result<Vec<u8>, E2eError> {
        let field = |name: &str| {
            header
                .get(name)
                .ok_or_else(|| E2eError::Header(format!("falta {name}")))
        };
        let epoch: u32 = field(HDR_E2E_EPOCH)?
            .parse()
            .map_err(|_| E2eError::Header(format!("{HDR_E2E_EPOCH} no es u32")))?;
        let counter: u64 = field(HDR_E2E_CTR)?
            .parse()
            .map_err(|_| E2eError::Header(format!("{HDR_E2E_CTR} no es u64")))?;
        let tag_v = hex::decode(field(HDR_E2E_TAG)?)
            .map_err(|_| E2eError::Header(format!("{HDR_E2E_TAG} no es hex")))?;
        let tag: [u8; aead::TAG_LEN] = tag_v
            .as_slice()
            .try_into()
            .map_err(|_| E2eError::Header(format!("{HDR_E2E_TAG} no mide {}", aead::TAG_LEN)))?;
        // La encarnación del emisor es la sesión de la ventana: un emisor que
        // reinicia vuelve al contador 1 con una encarnación nueva, y así sus
        // frames legítimos no parecen replays de la anterior.
        let session = u64::from_str_radix(field(crate::southbound::orr::HDR_INCARNATION)?, 16)
            .map_err(|_| E2eError::Header("incarnation no es hex de 64 bits".into()))?;

        let secret = {
            let g = self.peers.lock();
            g.get(source)
                .and_then(|st| st.secret(epoch))
                .ok_or_else(|| E2eError::UnknownEpoch {
                    peer: source.to_owned(),
                    epoch,
                })?
        };
        let sorted: BTreeMap<&str, &str> = header
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let aad = Self::aad(source, &self.my_id, sorted.into_iter());
        let (k, nonce) = Self::derive(&secret, key_id);
        let pt =
            aead::open_detached(&*k, &nonce, payload, &tag, &aad).map_err(|_| E2eError::BadTag)?;

        // Frescura: sólo con el tag ya verificado. Antes, cualquiera podría
        // tirar la ventana con una sesión inventada.
        {
            let mut g = self.peers.lock();
            let st = self.state(&mut g, source);
            st.replay.check_and_set(session, counter)?;
        }
        Ok(pt)
    }

    // ─── acuerdo de clave ───────────────────────────────────────────────

    /// Lado que pide: genera la keypair efímera, guarda la secreta y
    /// devuelve lo que hay que mandar.
    pub fn begin_request(&self, peer: &str) -> Result<KemRequest, E2eError> {
        let kem = pqc::kem_for(&self.cfg.suite).map_err(|e| E2eError::Kem(e.to_string()))?;
        let kp = kem.keygen().map_err(|e| E2eError::Kem(e.to_string()))?;
        {
            let mut g = self.peers.lock();
            let st = self.state(&mut g, peer);
            st.pending_esk = Some((Zeroizing::new(kp.secret), Instant::now()));
        }
        Ok(KemRequest {
            suite: kp.suite,
            epk: base64::engine::general_purpose::STANDARD.encode(kp.public),
        })
    }

    /// Lado que pide, al recibir la respuesta: descapsula con la secreta
    /// pendiente (que se destruye aquí) y adopta la época.
    pub fn complete_request(&self, peer: &str, resp: &KemResponse) -> Result<u32, E2eError> {
        if resp.epoch == 0 {
            return Err(E2eError::Peer("época 0 no es válida".into()));
        }
        // La suite viaja por el cable pero NO se negocia: se exige igual a la
        // local. Sin este suelo, un peer (autenticado, pero quizá mal
        // configurado o comprometido) podía bajar el par a ml-kem-512 y nadie
        // lo veía — en un fichero cuya premisa es que el que responde no
        // decide nada que importe. La deriva de config entre nodos se vuelve
        // un error a la vista en vez de un downgrade silencioso.
        if resp.suite != self.cfg.suite {
            return Err(E2eError::Peer(format!(
                "suite e2e '{}' del peer != '{}' local: [transport_e2e].suite debe ser \
                 idéntica en todo el despliegue",
                resp.suite, self.cfg.suite
            )));
        }
        let kem = pqc::kem_for(&resp.suite).map_err(|e| E2eError::Kem(e.to_string()))?;
        let ct = base64::engine::general_purpose::STANDARD
            .decode(&resp.ct)
            .map_err(|e| E2eError::Peer(format!("ct base64: {e}")))?;
        // La esk se saca bajo el lock y se descapsula fuera: el lock lo
        // comparten el generador y el pump de entregas en su camino caliente.
        let (esk, since) = {
            let mut g = self.peers.lock();
            self.state(&mut g, peer)
                .pending_esk
                .take()
                .ok_or_else(|| E2eError::Peer("respuesta sin acuerdo en vuelo".into()))?
        };
        if since.elapsed() > PENDING_ESK_TTL {
            return Err(E2eError::Peer("acuerdo en vuelo caducado".into()));
        }
        let ss = Zeroizing::new(
            kem.decap(&esk, &ct)
                .map_err(|e| E2eError::Kem(e.to_string()))?,
        );
        drop(esk);
        let mut g = self.peers.lock();
        self.state(&mut g, peer)
            .store(resp.epoch, &ss, self.cfg.epoch_history_keep)?;
        Ok(resp.epoch)
    }

    /// Lado que responde: encapsula contra la efímera del peer, asigna una
    /// época nueva y pasa a emitir con ella. `peer` viene del certificado,
    /// nunca del cuerpo.
    pub fn respond(&self, peer: &str, req: &KemRequest) -> Result<KemResponse, E2eError> {
        // Mismo suelo que en complete_request: la suite no se negocia.
        if req.suite != self.cfg.suite {
            return Err(E2eError::Peer(format!(
                "suite e2e '{}' del peer != '{}' local: [transport_e2e].suite debe ser \
                 idéntica en todo el despliegue",
                req.suite, self.cfg.suite
            )));
        }
        let kem = pqc::kem_for(&req.suite).map_err(|e| E2eError::Kem(e.to_string()))?;
        let epk = base64::engine::general_purpose::STANDARD
            .decode(&req.epk)
            .map_err(|e| E2eError::Peer(format!("epk base64: {e}")))?;
        if epk.len() != kem.public_key_len() {
            return Err(E2eError::Peer(format!(
                "epk de {} bytes, {} espera {}",
                epk.len(),
                req.suite,
                kem.public_key_len()
            )));
        }
        let enc = kem.encap(&epk).map_err(|e| E2eError::Kem(e.to_string()))?;
        let ss = Zeroizing::new(enc.shared_secret);
        let mut g = self.peers.lock();
        let st = self.state(&mut g, peer);
        let epoch = st.fresh_epoch();
        st.store(epoch, &ss, self.cfg.epoch_history_keep)?;
        info!(peer, epoch, "dkms.e2e época acordada (respondiendo)");
        Ok(KemResponse {
            suite: req.suite.clone(),
            epoch,
            ct: base64::engine::general_purpose::STANDARD.encode(enc.ciphertext),
        })
    }

    /// Lanza un acuerdo con `peer` en background si no hay otro en vuelo ni
    /// acaba de intentarse. Es lo que llaman el generador cuando no tiene
    /// época y el receptor cuando ve una que no conoce; ambos vuelven a
    /// intentarlo solos, así que aquí no hay bucle de reintentos.
    pub fn request_agreement(self: &Arc<Self>, peer: &str) {
        let Some(client) = self.client.clone() else {
            warn!(
                peer,
                "dkms.e2e sin cliente HTTP a peers: no puedo acordar clave"
            );
            return;
        };
        let Some(pc) = self.registry.get(peer) else {
            debug!(peer, "dkms.e2e peer sin endpoint HTTP conocido todavía");
            return;
        };
        {
            let mut g = self.peers.lock();
            let st = self.state(&mut g, peer);
            if st.inflight {
                return;
            }
            if let Some(t) = st.last_attempt {
                if t.elapsed() < AGREEMENT_MIN_INTERVAL {
                    return;
                }
            }
            st.inflight = true;
            st.last_attempt = Some(Instant::now());
        }
        let me = self.clone();
        let peer = peer.to_owned();
        tokio::spawn(async move {
            let r = async {
                let req = me.begin_request(&peer)?;
                let resp = client
                    .post_e2e_kem(&peer, &pc, &req)
                    .await
                    .map_err(|e| E2eError::Peer(format!("{e:#}")))?;
                me.complete_request(&peer, &resp)
            }
            .await;
            {
                let mut g = me.peers.lock();
                let st = me.state(&mut g, &peer);
                st.inflight = false;
                if r.is_err() {
                    st.pending_esk = None;
                }
            }
            match r {
                Ok(epoch) => info!(peer = %peer, epoch, "dkms.e2e época acordada (pidiendo)"),
                Err(e) => warn!(
                    peer = %peer,
                    error = %e,
                    "dkms.e2e acuerdo fallido; se reintenta con el siguiente envío"
                ),
            }
        });
    }

    /// Rotación por tiempo. Sólo el lex-menor de cada par la dispara, para no
    /// rotar dos veces por época. Va por HTTP petición/respuesta, así que no
    /// tiene el problema del enlace ocioso que se vio en el rekey del QKC.
    pub fn spawn_rekey_loop(self: Arc<Self>) {
        let rekey = Duration::from_secs(self.cfg.rekey_secs.max(1));
        let period = Duration::from_secs((self.cfg.rekey_secs / 4).clamp(5, 300));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let due: Vec<String> = {
                    let g = self.peers.lock();
                    g.iter()
                        .filter(|(peer, st)| {
                            self.my_id.as_str() < peer.as_str()
                                && st.established_at.is_some_and(|t| t.elapsed() >= rekey)
                        })
                        .map(|(p, _)| p.clone())
                        .collect()
                };
                for p in due {
                    debug!(peer = %p, "dkms.e2e rotación por tiempo");
                    self.request_agreement(&p);
                }
            }
        });
    }

    /// `(época de envío, nº de épocas guardadas)` para los logs de estado.
    pub fn snapshot(&self, peer: &str) -> (Option<u32>, usize) {
        let g = self.peers.lock();
        g.get(peer)
            .map(|st| (st.send_epoch, st.epochs.len()))
            .unwrap_or((None, 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PeerCfg;

    fn pair() -> (Arc<E2e>, Arc<E2e>) {
        pair_with(TransportE2eCfg::default())
    }

    fn pair_with(cfg: TransportE2eCfg) -> (Arc<E2e>, Arc<E2e>) {
        let reg = || Arc::new(PeerRegistry::from_config(HashMap::<String, PeerCfg>::new()));
        let a = Arc::new(E2e::new("dkms-a", cfg.clone(), None, reg()));
        let b = Arc::new(E2e::new("dkms-b", cfg, None, reg()));
        (a, b)
    }

    /// A pide, B responde, A completa — el camino HTTP sin el HTTP.
    fn agree(a: &E2e, b: &E2e) -> u32 {
        let req = a.begin_request("dkms-b").unwrap();
        let resp = b.respond("dkms-a", &req).unwrap();
        a.complete_request("dkms-b", &resp).unwrap()
    }

    /// La suite no se negocia: se exige igual a la local en los dos lados.
    /// Config desalineada => error a la vista; respuesta adulterada a una
    /// suite más débil => el que pidió la rechaza.
    #[test]
    fn a_mismatched_suite_is_rejected_on_both_sides() {
        let (a, b) = pair();
        let (weak, _) = pair_with(TransportE2eCfg {
            suite: common::crypto::pqc::suite::ML_KEM_512.to_owned(),
            ..TransportE2eCfg::default()
        });
        let req = weak.begin_request("dkms-b").unwrap();
        let err = b
            .respond("dkms-a", &req)
            .expect_err("suite distinta a la local");
        assert!(err.to_string().contains("transport_e2e"));

        let req = a.begin_request("dkms-b").unwrap();
        let mut resp = b.respond("dkms-a", &req).unwrap();
        resp.suite = common::crypto::pqc::suite::ML_KEM_512.to_owned();
        assert!(a.complete_request("dkms-b", &resp).is_err());
    }

    fn header(key_id: &str) -> BTreeMap<String, String> {
        let mut h = BTreeMap::new();
        h.insert("msg_type".into(), "DKMS_BUFFER".into());
        h.insert("key_id".into(), key_id.into());
        h.insert("sae_origin".into(), "dkms-a".into());
        h.insert("incarnation".into(), "00000000deadbeef".into());
        h.insert("ack_endpoint".into(), "10.0.0.1:9444".into());
        h
    }

    fn as_map(h: &BTreeMap<String, String>) -> HashMap<String, String> {
        h.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
    }

    #[test]
    fn roundtrip_keeps_payload_size_and_both_sides_agree() {
        let (a, b) = pair();
        let epoch = agree(&a, &b);
        assert_eq!(a.snapshot("dkms-b").0, Some(epoch));
        assert_eq!(b.snapshot("dkms-a").0, Some(epoch));

        let key = [7u8; 32];
        let mut h = header("k1");
        let ct = a.seal("dkms-b", "k1", &mut h, &key).unwrap();
        assert_eq!(ct.len(), 32, "el tag no va en el payload");
        assert_ne!(ct.as_slice(), &key[..]);
        assert_eq!(h[HDR_E2E_EPOCH], epoch.to_string());
        assert_eq!(h[HDR_E2E_CTR], "1");
        assert_eq!(h[HDR_E2E_TAG].len(), 32);

        let pt = b.open("dkms-a", &as_map(&h), "k1", &ct).unwrap();
        assert_eq!(pt, key);

        // Y en el otro sentido con la misma época: B también sella.
        let mut h2 = header("k2");
        h2.insert("sae_origin".into(), "dkms-b".into());
        let ct2 = b.seal("dkms-a", "k2", &mut h2, &key).unwrap();
        assert_eq!(a.open("dkms-b", &as_map(&h2), "k2", &ct2).unwrap(), key);
    }

    #[test]
    fn no_epoch_until_agreement() {
        let (a, _b) = pair();
        let mut h = header("k1");
        let e = a.seal("dkms-b", "k1", &mut h, &[0u8; 32]).unwrap_err();
        assert!(matches!(e, E2eError::NoEpoch { .. }));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let (a, b) = pair();
        agree(&a, &b);
        let mut h = header("k1");
        let mut ct = a.seal("dkms-b", "k1", &mut h, &[1u8; 32]).unwrap();
        ct[5] ^= 0x01;
        let e = b.open("dkms-a", &as_map(&h), "k1", &ct).unwrap_err();
        assert!(matches!(e, E2eError::BadTag));
    }

    #[test]
    fn tampered_header_is_rejected_before_it_can_act() {
        // La encarnación está en el AAD: cambiarla para provocar un borrado
        // de buffers en el receptor invalida el tag.
        let (a, b) = pair();
        agree(&a, &b);
        let mut h = header("k1");
        let ct = a.seal("dkms-b", "k1", &mut h, &[1u8; 32]).unwrap();
        let mut m = as_map(&h);
        m.insert("incarnation".into(), "0000000000000001".into());
        assert!(matches!(
            b.open("dkms-a", &m, "k1", &ct).unwrap_err(),
            E2eError::BadTag
        ));
        // También el ack_endpoint, aunque no sea secreto.
        let mut m = as_map(&h);
        m.insert("ack_endpoint".into(), "6.6.6.6:1".into());
        assert!(matches!(
            b.open("dkms-a", &m, "k1", &ct).unwrap_err(),
            E2eError::BadTag
        ));
    }

    #[test]
    fn replay_is_rejected_after_a_valid_open() {
        let (a, b) = pair();
        agree(&a, &b);
        let mut h = header("k1");
        let ct = a.seal("dkms-b", "k1", &mut h, &[1u8; 32]).unwrap();
        let m = as_map(&h);
        b.open("dkms-a", &m, "k1", &ct).unwrap();
        assert!(matches!(
            b.open("dkms-a", &m, "k1", &ct).unwrap_err(),
            E2eError::Replay(ReplayError::Replayed { .. })
        ));
    }

    /// La ventana anti-replay por peer emisor: lo que llega desordenado
    /// dentro de la anchura se acepta una vez; lo que queda por debajo del
    /// suelo se rechaza como viejo aunque nunca se hubiera visto. Espejo de
    /// las pruebas de `frame_mac::ReplayWindow`, aquí sobre `seal`/`open`.
    #[test]
    fn replay_window_accepts_out_of_order_within_width_and_rejects_below_the_floor() {
        let (a, b) = pair_with(TransportE2eCfg {
            replay_window: 4,
            ..TransportE2eCfg::default()
        });
        agree(&a, &b);
        // La anchura pedida (4) queda por debajo del suelo de la ventana
        // (64, ver `frame_mac::ReplayWindow::new`): hacen falta más de 64
        // contadores para dejar algo por debajo del suelo.
        let sealed: Vec<_> = (1..=70u8)
            .map(|i| {
                let id = format!("k{i}");
                let mut h = header(&id);
                let ct = a.seal("dkms-b", &id, &mut h, &[i; 32]).unwrap();
                (id, as_map(&h), ct)
            })
            .collect();
        let open = |i: usize| {
            let (id, m, ct) = &sealed[i];
            b.open("dkms-a", m, id, ct)
        };
        // Llega primero el último: el suelo de la ventana sube con él.
        assert_eq!(open(69).unwrap(), [70u8; 32]);
        // Los que quedan dentro de la anchura entran, desordenados y una vez.
        assert_eq!(open(65).unwrap(), [66u8; 32]);
        assert!(matches!(
            open(65).unwrap_err(),
            E2eError::Replay(ReplayError::Replayed { .. })
        ));
        // Los de más abajo del suelo, nunca vistos, se rechazan por viejos.
        assert!(matches!(
            open(0).unwrap_err(),
            E2eError::Replay(ReplayError::TooOld { .. })
        ));
    }

    #[test]
    fn unknown_epoch_after_receiver_restart() {
        let (a, b) = pair();
        agree(&a, &b);
        let mut h = header("k1");
        let ct = a.seal("dkms-b", "k1", &mut h, &[1u8; 32]).unwrap();
        // B "reinicia": estado nuevo sin épocas.
        let (_, b2) = pair();
        assert!(matches!(
            b2.open("dkms-a", &as_map(&h), "k1", &ct).unwrap_err(),
            E2eError::UnknownEpoch { .. }
        ));
        // B2 pide; A, que responde, pasa a emitir con la época nueva y B2 abre.
        let req = b2.begin_request("dkms-a").unwrap();
        let resp = a.respond("dkms-b", &req).unwrap();
        b2.complete_request("dkms-a", &resp).unwrap();
        assert_eq!(a.snapshot("dkms-b").0, Some(resp.epoch));
        let mut h = header("k2");
        let ct = a.seal("dkms-b", "k2", &mut h, &[2u8; 32]).unwrap();
        assert_eq!(
            b2.open("dkms-a", &as_map(&h), "k2", &ct).unwrap(),
            [2u8; 32]
        );
    }

    #[test]
    fn concurrent_agreements_yield_distinct_epochs_not_one_with_two_secrets() {
        let (a, b) = pair();
        let ra = a.begin_request("dkms-b").unwrap();
        let rb = b.begin_request("dkms-a").unwrap();
        let sa = b.respond("dkms-a", &ra).unwrap();
        let sb = a.respond("dkms-b", &rb).unwrap();
        assert_ne!(sa.epoch, sb.epoch);
        a.complete_request("dkms-b", &sa).unwrap();
        b.complete_request("dkms-a", &sb).unwrap();
        // Las dos épocas abren en ambos lados, sea cual sea la que use cada uno.
        for kid in ["x", "y"] {
            let mut h = header(kid);
            let ct = a.seal("dkms-b", kid, &mut h, &[9u8; 32]).unwrap();
            assert_eq!(b.open("dkms-a", &as_map(&h), kid, &ct).unwrap(), [9u8; 32]);
        }
    }

    #[test]
    fn old_epochs_are_pruned_to_history() {
        let (a, b) = pair();
        let keep = TransportE2eCfg::default().epoch_history_keep;
        let first = agree(&a, &b);
        for _ in 0..keep {
            agree(&a, &b);
        }
        assert_eq!(a.snapshot("dkms-b").1, keep);
        // Sellar con la primera ya no es posible: no la guardamos.
        assert!(a
            .peers
            .lock()
            .get("dkms-b")
            .unwrap()
            .secret(first)
            .is_none());
    }

    #[test]
    fn wrong_key_id_changes_key_and_nonce() {
        let (a, b) = pair();
        agree(&a, &b);
        let mut h = header("k1");
        let ct = a.seal("dkms-b", "k1", &mut h, &[1u8; 32]).unwrap();
        assert!(matches!(
            b.open("dkms-a", &as_map(&h), "k-otro", &ct).unwrap_err(),
            E2eError::BadTag
        ));
    }
}
