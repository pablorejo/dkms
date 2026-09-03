//! Onion routing v3 con master_secret pre-compartido + AES-256-GCM.
//!
//! Cada par de ORRs comparte un `master_secret` de 32 B (vía ML-KEM
//! encap al arrancar, RPC `EstablishSecret`). Por cada capa, el origen
//! elige un `key_id` UUID v4 y deriva una clave AEAD de 32 B:
//!
//! ```text
//! K = HKDF-SHA256(salt=b"orr.onion.v1",
//!                 ikm=master_secret,
//!                 info=key_id ‖ b"orr.onion.v3",
//!                 L=32)
//! ```
//!
//! y sella con AES-256-GCM en modo *detached*: el payload del wire es SOLO el
//! ciphertext (mismo tamaño que el plaintext), el tag viaja aparte —en
//! `OrrHeader.tag` la capa externa, en `InnerLayer.tag` las internas— y el
//! nonce ni siquiera viaja: se deriva de `key_id` junto a la K
//! (`layer_nonce`), así que cada K se usa con exactamente un nonce y el reúso
//! GCM es imposible por construcción. Todo byte añadido al payload cuesta
//! material QKD por salto (el chunker OTP gasta una clave por bloque de
//! `key_size_bits/8`): meter `nonce ‖ tag` dentro midió un −51 % en el brazo
//! QKD, y por eso van fuera. El receptor, que tiene el mismo
//! `master_secret`, deriva la misma K y nonce con el `key_id` (cleartext en
//! `header_orr_mp`) y abre.
//!
//! ## Por qué AEAD y no XOR
//!
//! Hasta 2026-08-28 esto era `plaintext ⊕ HKDF(master_secret, key_id, len)`:
//! confidencialidad correcta y **cero integridad**, porque XOR es maleable.
//! Cualquiera que pudiera tocar el ciphertext —un QKC del camino, que lo ve en
//! claro entre descifrar y recifrar— podía aplicarle un delta arbitrario, y lo
//! único que quedaba enfrente era el `key_digest` del DKMS: un SHA-256 **sin
//! clave** que sólo funciona porque viaja dentro del cifrado, es decir,
//! integridad apoyada en la confidencialidad. Y un `master_secret` divergente
//! entregaba basura al DKMS en vez de dar error.
//!
//! Con AEAD el tag es la integridad y la autenticación de origen de la capa a
//! la vez, con la clave del par de ORRs, y no dependen de nada más. Además sale
//! más barato: antes se expandía HKDF byte a byte sobre todo el mensaje; ahora
//! son 32 B de HKDF y AES-NI para el resto.
//!
//! El `aad` ata la capa a lo que la acompaña **en claro** —`key_id`,
//! `epoch_id`, `max_hops`, `session ‖ counter` (la frescura del origen, ver
//! `onion_replay`) y la cabecera DKMS—, de modo que un QKC del camino no
//! puede mover una capa válida a otra época, cambiarle el `max_hops` para que
//! el destino la entregue en vez de reenviarla, rejuvenecer un frame viejo ni
//! recolocar la cebolla bajo otra cabecera DKMS.
//!
//! ## Construcción multi-capa
//!
//! Para path `[X1, X2, ..., Xn]` (X_n = destino final):
//!
//! ```text
//! innermost = seal(K_{O,Xn}, body,                              aad(kid_n, ep_n, 0))
//! layer_{n-1} = seal(K_{O,X_{n-1}},
//!                    msgpack(Inner{next=Xn, kid=kid_n, ct=innermost}),
//!                    aad(kid_{n-1}, ep_{n-1}, 1))
//! ...
//! outermost  = seal(K_{O,X1},
//!                   msgpack(Inner{next=X2, kid=kid_2, ct=layer_2}),
//!                   aad(kid_1, ep_1, n-1))
//! ```
//!
//! El header del wire frame externo lleva `from=O`, `to=Xn`,
//! `next_orr_id=X1`, `key_id=kid_1`, `max_hops=n-1`. El `payload` del
//! wire frame es `outermost`. El QKC lo OTP-cifra en cada enlace
//! QKC↔QKC del path —y desde 2026-08-28 le pone además su propio MAC de
//! enlace, que cubre al atacante del cable; esta capa cubre lo que aquél no
//! puede: el camino extremo a extremo entre los dos ORRs.
//!
//! ## Peeling
//!
//! El ORR X1 recibe del QKC:
//!
//! 1. Lee header → `next_orr_id == self`, `key_id`, `max_hops`,
//!    `from`, `epoch_id`. Busca `master_secret` indexado por `from` y época.
//! 2. `K = HKDF(master_secret, key_id)`.
//! 3. `inner = open(K, payload, aad(key_id, epoch_id, max_hops))`. Si el tag no
//!    cuadra, **error** — no un plaintext raro.
//! 4. Si `max_hops > 0`: inner es `InnerLayer { next=X2, kid=kid_2,
//!    ct=layer_2 }`. Construye nuevo wire frame con header
//!    reescrito y reenvía al QKC del X2.
//! 5. Si `max_hops == 0`: inner es `body_dkms` directo. Entrega local.
//!
//! ## Tamaño (vs. esquema v2)
//!
//! La capa externa no añade NADA al payload (ciphertext = tamaño del
//! plaintext; su tag va en `OrrHeader.tag` y el nonce se deriva): en modo 1,
//! un body de 32 B viaja como payload de 32 B y el OTP gasta exactamente lo
//! mismo que en passthrough. Cada capa **interna** (modos ≥2 / -1) envuelve
//! la anterior en un `InnerLayer` msgpack —`next_orr_id`, `epoch_id`,
//! `key_id` (18 B como `bin`), su `tag` (16 B) y el ciphertext— unas decenas
//! de bytes por hop: crecimiento **lineal** en nº de hops. El esquema v2
//! (`Vec<kem_ct>` por capa, ~1,1 KB cada uno) explotaba a ~4 MB en modo -1.

use common::crypto::aead::TAG_LEN;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::error::{OrrError, Result};

const HKDF_SALT: &[u8] = b"orr.onion.v1";

/// Una capa interna ya cifrada, lista para meter en el `payload` del
/// wire frame de la capa siguiente (más externa).
///
/// `key_id` y `xor_ct` usan `serde_bytes` para que msgpack los serialice
/// como tipo `bin` (1 byte por byte) en lugar de array (1-2 bytes por
/// byte). Sin esto, con bytes pseudo-aleatorios, ~50 % de los bytes
/// inflan a 2 bytes y duplican el overhead per-layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InnerLayer {
    /// ORR destino de la siguiente capa (el peeler que mirará `key_id`
    /// para derivar K y descifrar `xor_ct`).
    pub next_orr_id: String,
    /// Época del `master_secret` con la que se cifró `xor_ct` para el
    /// peeler de la siguiente capa. Lo usa el siguiente ORR para
    /// resolver `peers.master_for_epoch(from, epoch_id)` cuando
    /// reescribe el wire frame al reenviar (audit H-3 / Option B). `0`
    /// significa "pre-rotación" / passthrough sin epoch (compat con
    /// la semántica v2 a través del alias `put_master_secret`).
    #[serde(default)]
    pub epoch_id: u32,
    /// UUID v4 raw (16 B) de la K de la siguiente capa.
    #[serde(with = "serde_bytes")]
    pub key_id: [u8; 16],
    /// Ciphertext de la capa siguiente, **sin** nonce ni tag: los dos van
    /// aparte para no alargar el payload (ver `seal_layer`). El nombre se queda
    /// por compatibilidad del msgpack `named` (era el `K ⊕ plaintext` del
    /// esquema XOR anterior).
    #[serde(with = "serde_bytes")]
    pub xor_ct: Vec<u8>,
    /// Tag AES-GCM de la capa siguiente. Va aquí, no pegado a `xor_ct`, por la
    /// misma razón: el troceado del OTP cuenta bytes.
    #[serde(with = "serde_bytes", default)]
    pub tag: [u8; TAG_LEN],
}

impl InnerLayer {
    pub fn encode(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self).map_err(|e| OrrError::Relay(format!("inner encode: {e}")))
    }

    pub fn decode(buf: &[u8]) -> Result<Self> {
        rmp_serde::from_slice(buf).map_err(|e| OrrError::Relay(format!("inner decode: {e}")))
    }
}

/// Hop en el path del onion: orr_id + master_secret + epoch_id
/// compartido (lo que produjo la última rotación con ese peer, o el
/// bootstrap inicial cuando aún no hay rotación). El caller
/// (service.rs) lo construye consultando `PeerRegistry`:
/// `epoch_id = latest_epoch_for(orr_id)?` y
/// `master_secret = master_for_epoch(orr_id, epoch_id)?`.
///
/// `master_secret` va en `Zeroizing<[u8; 32]>` para que se borre al
/// drop del struct, aun cuando esta vida sea transitoria (sólo dura
/// la llamada a `build_onion`). Política CLAUDE.md "RAM-only +
/// zeroize" + audit H-3 criterio #5.
#[derive(Clone)]
pub struct PathHopSecret {
    pub orr_id: String,
    pub master_secret: Zeroizing<[u8; 32]>,
    /// Época del `master_secret` que se va a usar para cifrar esta
    /// capa. Acompaña al `key_id` en el wire (capa externa) o en la
    /// `InnerLayer` previa (capas intermedias).
    pub epoch_id: u32,
}

impl std::fmt::Debug for PathHopSecret {
    // Redactado (auditoría 2026-09-03, R11): un `{:?}` de un camino no
    // puede volcar 32 B de secreto por salto.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PathHopSecret")
            .field("orr_id", &self.orr_id)
            .field("master_secret", &"<redacted>")
            .field("epoch_id", &self.epoch_id)
            .finish()
    }
}

/// Output de `build_onion`: lo que el caller necesita para armar el
/// wire frame externo.
#[derive(Debug, Clone)]
pub struct OnionWire {
    /// Primer hop del path (= `header.next_orr_id` del wire frame).
    pub first_hop_orr: String,
    /// Época del `master_secret` con la que se cifró la capa más
    /// externa. El caller la copia a `Frame.epoch_id` del wire frame
    /// (audit H-3): el primer hop lee `frame.epoch_id` para resolver
    /// `peers.master_for_epoch(from, epoch_id)`.
    pub first_epoch_id: u32,
    /// `key_id` UUID v4 de la capa más externa (= `header.key_id`).
    pub first_key_id: [u8; 16],
    /// Hops onion restantes tras pelar la capa externa (= `header.max_hops`).
    /// Si el path tiene N hops, este valor es N-1.
    pub max_hops: i32,
    /// El ciphertext de la capa externa. Va en el `payload` del wire frame, y
    /// mide lo mismo que el plaintext: el tag va aparte, en la cabecera.
    pub payload: Vec<u8>,
    /// Tag AES-GCM de la capa externa (= `header.tag` del wire frame).
    pub tag: [u8; TAG_LEN],
    /// Encarnación del ORR de origen, aleatoria por arranque de proceso.
    /// Va en la cabecera y en el AAD de todas las capas.
    pub session: u64,
    /// Contador monotónico del origen, uno por mensaje. Idem.
    pub counter: u64,
}

/// Resultado de pelar una capa.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Peeled {
    /// Capa intermedia: el caller construye un nuevo wire frame con
    /// `header.next_orr_id=inner.next_orr_id, key_id=inner.key_id,
    /// max_hops=prev_max_hops-1, payload=inner.xor_ct`.
    Forward(InnerLayer),
    /// Capa terminal: bytes del body_dkms directos para entregar.
    Deliver(Vec<u8>),
}

/// Etiqueta de versión del AAD de una capa. Cambiarla invalida las capas
/// construidas con el esquema anterior, que es lo que se quiere: un despliegue
/// mezclado tiene que fallar ruidoso.
const AAD_V3: &[u8] = b"orr.onion.v3";

/// AAD de una capa: ata la capa a la cabecera EN CLARO que la acompaña.
///
/// Sin esto el tag protege el contenido pero no su contexto, y un QKC en el
/// camino podría mover una capa válida a otra época o cambiarle el `max_hops`
/// para que el destino la entregue en vez de reenviarla (o al revés).
fn layer_aad(
    key_id: &[u8; 16],
    epoch_id: u32,
    max_hops: i32,
    session: u64,
    counter: u64,
    dkms_hdr: &[u8],
) -> Vec<u8> {
    let mut aad = Vec::with_capacity(AAD_V3.len() + 16 + 4 + 4 + 8 + 8 + 32);
    aad.extend_from_slice(AAD_V3);
    aad.extend_from_slice(key_id);
    aad.extend_from_slice(&epoch_id.to_be_bytes());
    aad.extend_from_slice(&max_hops.to_be_bytes());
    // Frescura: los pone el ORR de ORIGEN, uno por mensaje, y van iguales en
    // todas las capas. Un ORR que reenvía tiene que copiarlos a la cabecera que
    // escribe; si los cambia, este AAD deja de cuadrar en el siguiente salto y
    // el peel falla. Ver `onion_replay`.
    aad.extend_from_slice(&session.to_be_bytes());
    aad.extend_from_slice(&counter.to_be_bytes());
    // El header DKMS viaja EN CLARO al lado del payload y el QKC lo propaga
    // byte a byte sin mirarlo, así que en un camino multi-salto un QKC
    // intermedio podía reescribirlo a placer: cambiar el `incarnation` hace que
    // el DKMS destino tire sus buffers para ese peer, y cambiar el
    // `ack_endpoint` redirige los ACK. El `key_digest` que lleva sólo ata
    // `key_id ‖ bytes`, no el resto del header. Atándolo aquí, cualquier
    // cambio rompe el tag de la capa.
    let mut h = <Sha256 as sha2::Digest>::new();
    sha2::Digest::update(&mut h, dkms_hdr);
    aad.extend_from_slice(&sha2::Digest::finalize(h));
    aad
}

/// Clave AEAD de una capa: 32 B de HKDF, no un keystream del tamaño del cuerpo.
/// Con AEAD el cifrado lo hace AES-256-GCM, así que basta la clave — y sale más
/// barato que expandir HKDF byte a byte sobre todo el mensaje.
fn layer_key(master_secret: &[u8; 32], key_id: &[u8; 16]) -> Zeroizing<[u8; 32]> {
    let mut k = Zeroizing::new([0u8; 32]);
    let hk = Hkdf::<Sha256>::new(Some(HKDF_SALT), master_secret);
    let mut info = [0u8; 16 + AAD_V3.len()];
    info[..16].copy_from_slice(key_id);
    info[16..].copy_from_slice(AAD_V3);
    hk.expand(&info, k.as_mut())
        .expect("32 B is a valid HKDF-SHA256 output length");
    k
}

/// Nonce de una capa, derivado del `key_id`.
///
/// No hace falta transmitirlo, y es lo más seguro que se puede hacer aquí: la
/// clave AEAD sale de `HKDF(master_secret, key_id)`, así que cada `key_id` da
/// una clave DISTINTA. Derivando el nonce del mismo `key_id`, cada clave se usa
/// con exactamente un nonce y la reutilización —que en GCM es catastrófica— es
/// imposible por construcción, no por disciplina del caller.
fn layer_nonce(key_id: &[u8; 16]) -> [u8; common::crypto::aead::NONCE_LEN] {
    let mut n = [0u8; common::crypto::aead::NONCE_LEN];
    n.copy_from_slice(&key_id[..common::crypto::aead::NONCE_LEN]);
    n
}

/// Cifra una capa con AES-256-GCM. Devuelve `(ciphertext, tag)` por separado.
///
/// Antes esto era un XOR con un keystream HKDF: confidencialidad perfecta en su
/// clase y **cero integridad**, porque XOR es maleable. La única comprobación
/// que había extremo a extremo era el `key_digest` del DKMS, un SHA-256 **sin
/// clave** que sólo funciona porque viaja dentro del cifrado.
///
/// El tag sale **separado** y no se anexa al payload, y el nonce ni se
/// transmite: ninguno de los dos es secreto, y meterlos en el payload cuesta
/// claves QKD. El OTP del enlace trocea en bloques de `key_size_bits / 8` y
/// gasta una clave por bloque, así que 28 bytes de más en un mensaje de 32
/// obligan a un segundo bloque — el doble de material QKD por salto. Medido en
/// CESGA el 2026-08-28: −51 % de rendimiento, con `wenc_to=61551` y
/// `misses=369171` donde antes había ceros. Así el ciphertext mide exactamente
/// lo que el plaintext y el troceado no cambia.
#[allow(clippy::too_many_arguments)]
fn seal_layer(
    master_secret: &[u8; 32],
    key_id: &[u8; 16],
    plaintext: &[u8],
    epoch_id: u32,
    max_hops: i32,
    session: u64,
    counter: u64,
    dkms_hdr: &[u8],
) -> Result<(Vec<u8>, [u8; TAG_LEN])> {
    let k = layer_key(master_secret, key_id);
    let aad = layer_aad(key_id, epoch_id, max_hops, session, counter, dkms_hdr);
    Ok(common::crypto::aead::seal_detached(
        k.as_ref(),
        &layer_nonce(key_id),
        plaintext,
        &aad,
    )?)
}

/// Inversa de [`seal_layer`]. Un tag que no cuadra es un error, no un
/// plaintext raro: es justo la diferencia con el XOR de antes.
#[allow(clippy::too_many_arguments)]
fn open_layer(
    master_secret: &[u8; 32],
    key_id: &[u8; 16],
    ct: &[u8],
    tag: &[u8; TAG_LEN],
    epoch_id: u32,
    max_hops: i32,
    session: u64,
    counter: u64,
    dkms_hdr: &[u8],
) -> Result<Vec<u8>> {
    let k = layer_key(master_secret, key_id);
    let aad = layer_aad(key_id, epoch_id, max_hops, session, counter, dkms_hdr);
    Ok(common::crypto::aead::open_detached(
        k.as_ref(),
        &layer_nonce(key_id),
        ct,
        tag,
        &aad,
    )?)
}

/// Construye un onion completo dado el `path` (lista ordenada de
/// `PathHopSecret`, primer elemento = primer hop, último = destino
/// final). Devuelve el `OnionWire` con todo lo que el caller necesita,
/// incluido el `first_epoch_id` que va a `Frame.epoch_id` en el wire.
///
/// El `epoch_id` se propaga capa-a-capa: la `InnerLayer` que
/// recibe el hop `path[i]` lleva el `epoch_id` de `path[i+1]` (el
/// peeler de la siguiente capa lo usa para descifrar su propia
/// `xor_ct`). El `epoch_id` del primer hop sale en
/// `OnionWire.first_epoch_id`.
pub fn build_onion(
    path: &[PathHopSecret],
    body: Vec<u8>,
    session: u64,
    counter: u64,
    dkms_hdr: &[u8],
) -> Result<OnionWire> {
    if path.is_empty() {
        return Err(OrrError::Relay("onion: empty path".into()));
    }

    // Capa más interna: cifra body con K_{O,dst}. La época del
    // destino se "consume" al envolver la siguiente capa hacia fuera
    // (acaba en la InnerLayer.epoch_id que llega al penúltimo hop).
    let dst = path.last().unwrap();
    let kid_dst = *Uuid::new_v4().as_bytes();
    // La capa del destino se pela con max_hops = 0 (entrega), y con la época
    // del propio destino: los dos entran en el AAD, así que el tag ata la capa
    // a la posición que le toca en el path.
    let (mut current_xor, mut current_tag) = seal_layer(
        &dst.master_secret,
        &kid_dst,
        &body,
        dst.epoch_id,
        0,
        session,
        counter,
        dkms_hdr,
    )?;
    let mut current_next_orr_id = dst.orr_id.clone();
    let mut current_next_epoch = dst.epoch_id;
    let mut current_key_id = kid_dst;

    // Reenvolver hacia fuera: cada hop intermedio recibe una capa cuyo
    // plaintext es un `InnerLayer { next_orr_id, epoch_id, key_id,
    // xor_ct }` apuntando a la siguiente capa.
    for i in (0..path.len() - 1).rev() {
        let hop = &path[i];
        let layer = InnerLayer {
            next_orr_id: current_next_orr_id,
            epoch_id: current_next_epoch,
            key_id: current_key_id,
            xor_ct: current_xor,
            tag: current_tag,
        };
        let layer_pt = layer.encode()?;
        let kid_hop = *Uuid::new_v4().as_bytes();
        // El hop i pela su capa con max_hops = (len-1) - i: los que le quedan
        // al onion por delante.
        let hops_left = (path.len() - 1 - i) as i32;
        let sealed = seal_layer(
            &hop.master_secret,
            &kid_hop,
            &layer_pt,
            hop.epoch_id,
            hops_left,
            session,
            counter,
            dkms_hdr,
        )?;
        current_xor = sealed.0;
        current_tag = sealed.1;
        current_next_orr_id = hop.orr_id.clone();
        current_next_epoch = hop.epoch_id;
        current_key_id = kid_hop;
    }

    Ok(OnionWire {
        first_hop_orr: current_next_orr_id,
        first_epoch_id: current_next_epoch,
        first_key_id: current_key_id,
        max_hops: (path.len() as i32) - 1,
        payload: current_xor,
        tag: current_tag,
        session,
        counter,
    })
}

/// Pela una capa.
///
/// `master_secret`: el shared secret de 32 B con `header.from` (lookup
/// en `PeerRegistry::master_secret`).
/// `key_id`: del `header.key_id` del wire frame entrante.
/// `xor_ct`: el `payload` del wire frame entrante (ya descifrado por el
/// QKC en el último hop QKC-OTP).
/// `max_hops`: del `header.max_hops` del wire frame entrante. Si > 0
/// devuelve `Forward(InnerLayer)`; si == 0 devuelve `Deliver(body)`.
#[allow(clippy::too_many_arguments)]
pub fn peel(
    master_secret: &[u8; 32],
    key_id: &[u8; 16],
    xor_ct: &[u8],
    tag: &[u8; TAG_LEN],
    max_hops: i32,
    epoch_id: u32,
    session: u64,
    counter: u64,
    dkms_hdr: &[u8],
) -> Result<Peeled> {
    let pt = open_layer(
        master_secret,
        key_id,
        xor_ct,
        tag,
        epoch_id,
        max_hops,
        session,
        counter,
        dkms_hdr,
    )?;
    if max_hops <= 0 {
        Ok(Peeled::Deliver(pt))
    } else {
        let layer = InnerLayer::decode(&pt)?;
        Ok(Peeled::Forward(layer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    fn random_secret() -> [u8; 32] {
        let mut s = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut s);
        s
    }

    #[test]
    fn aead_round_trip_does_not_grow_the_payload() {
        // LA propiedad que hace viable esto sobre un enlace QKD: el ciphertext
        // mide EXACTAMENTE lo que el plaintext. El tag va aparte y el nonce se
        // deriva del key_id, así que el troceado del OTP no cambia y no se
        // gasta una segunda clave por salto.
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let body = b"hola mundo onion v3".to_vec();
        let (ct, tag) = seal_layer(&ms, &kid, &body, 5, 1, 9, 4, b"h").unwrap();
        assert_eq!(ct.len(), body.len(), "el payload no debe crecer");
        assert_eq!(tag.len(), TAG_LEN);
        assert_eq!(
            open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"h").unwrap(),
            body
        );
    }

    #[test]
    fn aead_catches_a_tampered_layer() {
        // Con el XOR de antes esto pasaba desapercibido: un bit volteado en el
        // ciphertext salía como un bit volteado en el plaintext, y la única
        // defensa era el `key_digest` sin clave del DKMS.
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let body = b"contenido que no debe poder cambiarse".to_vec();
        let (mut ct, tag) = seal_layer(&ms, &kid, &body, 5, 1, 9, 4, b"h").unwrap();
        ct[3] ^= 0x01;
        assert!(open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"h").is_err());
    }

    #[test]
    fn aead_catches_a_tampered_tag() {
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let (ct, mut tag) = seal_layer(&ms, &kid, b"capa", 5, 1, 9, 4, b"h").unwrap();
        tag[0] ^= 0xFF;
        assert!(open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"h").is_err());
    }

    #[test]
    fn aead_binds_the_cleartext_header() {
        // El AAD ata época y max_hops, que viajan EN CLARO en la cabecera. Sin
        // eso, un QKC del camino podría mover una capa válida a otra época, o
        // cambiar el max_hops para que el destino entregue en vez de reenviar.
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let (ct, tag) = seal_layer(&ms, &kid, b"capa", 5, 1, 9, 4, b"h").unwrap();
        assert!(
            open_layer(&ms, &kid, &ct, &tag, 6, 1, 9, 4, b"h").is_err(),
            "época cambiada"
        );
        assert!(
            open_layer(&ms, &kid, &ct, &tag, 5, 0, 9, 4, b"h").is_err(),
            "max_hops cambiado"
        );
        // Y el key_id, que selecciona la clave Y el nonce, también está atado.
        let other_kid = *Uuid::new_v4().as_bytes();
        assert!(open_layer(&ms, &other_kid, &ct, &tag, 5, 1, 9, 4, b"h").is_err());
    }

    #[test]
    fn aead_binds_the_freshness_pair() {
        // Si session/counter no entrasen en el AAD, un ORR que reenvía podría
        // cambiarlos y saltarse la ventana anti-replay del destino.
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let (ct, tag) = seal_layer(&ms, &kid, b"capa", 5, 1, 9, 4, b"h").unwrap();
        assert!(
            open_layer(&ms, &kid, &ct, &tag, 5, 1, 10, 4, b"h").is_err(),
            "session"
        );
        assert!(
            open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 5, b"h").is_err(),
            "counter"
        );
        assert!(open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"h").is_ok());
    }

    #[test]
    fn aead_binds_the_cleartext_dkms_header() {
        // El header DKMS lleva `incarnation` (cambiarlo hace que el destino
        // tire sus buffers para ese peer) y `ack_endpoint`. Viaja en claro y el
        // QKC lo propaga sin mirarlo, así que en multi-salto un QKC intermedio
        // podía reescribirlo. Atado al AAD, cambiarlo rompe el tag.
        let ms = random_secret();
        let kid = *Uuid::new_v4().as_bytes();
        let (ct, tag) = seal_layer(&ms, &kid, b"capa", 5, 1, 9, 4, b"incarnation=1").unwrap();
        assert!(open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"incarnation=2").is_err());
        assert!(open_layer(&ms, &kid, &ct, &tag, 5, 1, 9, 4, b"incarnation=1").is_ok());
    }

    #[test]
    fn aead_rejects_a_foreign_secret() {
        let kid = *Uuid::new_v4().as_bytes();
        let (ct, tag) = seal_layer(&random_secret(), &kid, b"x", 1, 0, 1, 1, b"h").unwrap();
        assert!(open_layer(&random_secret(), &kid, &ct, &tag, 1, 0, 1, 1, b"h").is_err());
    }

    #[test]
    fn every_layer_gets_its_own_nonce() {
        // El nonce sale del key_id, y el key_id es un UUIDv4 fresco por capa,
        // así que dos capas nunca comparten (clave, nonce) — que en GCM es la
        // única forma de romperlo del todo.
        let a = *Uuid::new_v4().as_bytes();
        let b = *Uuid::new_v4().as_bytes();
        assert_ne!(layer_nonce(&a), layer_nonce(&b));
        // Y el mismo cuerpo con key_ids distintos da ciphertexts distintos.
        let ms = random_secret();
        let (ca, _) = seal_layer(&ms, &a, b"mismo cuerpo", 1, 0, 1, 1, b"h").unwrap();
        let (cb, _) = seal_layer(&ms, &b, b"mismo cuerpo", 1, 0, 1, 1, b"h").unwrap();
        assert_ne!(ca, cb);
    }

    /// Guarda de regresión del fallo medido en CESGA el 2026-08-28.
    ///
    /// El OTP del enlace QKC trocea el payload en bloques de
    /// `key_size_bits / 8` —32 B por defecto— y gasta UNA CLAVE QKD POR BLOQUE.
    /// Cuando el AEAD metía `nonce ‖ ct ‖ tag` dentro del payload, un mensaje
    /// de 32 B pasaba a 60: de un bloque a dos, el doble de material por salto,
    /// y −51 % de rendimiento en el brazo QKD. En PQC no se habría visto.
    ///
    /// Si alguien vuelve a meter bytes en el payload, este test lo dice aquí en
    /// vez de dentro de un job de una hora.
    #[test]
    fn the_payload_must_not_grow_past_the_otp_block() {
        const OTP_BLOCK: usize = 32; // key_size_bits (256) / 8
        let body = vec![0xAAu8; OTP_BLOCK];
        let path = vec![PathHopSecret {
            orr_id: "orr_dst".into(),
            master_secret: Zeroizing::new(random_secret()),
            epoch_id: 7,
        }];
        let onion = build_onion(&path, body.clone(), 42, 1, b"hdr").unwrap();
        assert_eq!(
            onion.payload.len(),
            body.len(),
            "el payload de la cebolla no puede crecer: cada byte de más puede \
             costar una clave QKD entera por salto"
        );
        assert_eq!(
            onion.payload.len().div_ceil(OTP_BLOCK),
            1,
            "un mensaje de un bloque tiene que seguir siendo un bloque"
        );
        // El tag existe, pero fuera del payload: va en la cabecera.
        assert_eq!(onion.tag.len(), TAG_LEN);
    }

    #[test]
    fn build_and_peel_1_hop() {
        let ms_dst = random_secret();
        let body = b"single hop test".to_vec();
        let path = vec![PathHopSecret {
            orr_id: "orr_dst".into(),
            master_secret: Zeroizing::new(ms_dst),
            epoch_id: 7,
        }];
        let onion = build_onion(&path, body.clone(), 42, 1, b"hdr").unwrap();
        assert_eq!(onion.first_hop_orr, "orr_dst");
        assert_eq!(onion.first_epoch_id, 7);
        assert_eq!(onion.max_hops, 0);
        let peeled = peel(
            &ms_dst,
            &onion.first_key_id,
            &onion.payload,
            &onion.tag,
            0,
            7,
            42,
            1,
            b"hdr",
        )
        .unwrap();
        assert_eq!(peeled, Peeled::Deliver(body));
    }

    #[test]
    fn build_and_peel_3_hops_threads_epoch() {
        let ms_b = random_secret();
        let ms_c = random_secret();
        let ms_d = random_secret();
        let body = b"viaje cebolla 3 hops".to_vec();
        let path = vec![
            PathHopSecret {
                orr_id: "orr_b".into(),
                master_secret: Zeroizing::new(ms_b),
                epoch_id: 11,
            },
            PathHopSecret {
                orr_id: "orr_c".into(),
                master_secret: Zeroizing::new(ms_c),
                epoch_id: 22,
            },
            PathHopSecret {
                orr_id: "orr_d".into(),
                master_secret: Zeroizing::new(ms_d),
                epoch_id: 33,
            },
        ];
        let onion = build_onion(&path, body.clone(), 42, 1, b"hdr").unwrap();
        assert_eq!(onion.first_hop_orr, "orr_b");
        // El epoch que va al wire (Frame.epoch_id) es el del primer hop.
        assert_eq!(onion.first_epoch_id, 11);
        assert_eq!(onion.max_hops, 2);

        // B pela: max_hops=2 ⇒ Forward(InnerLayer apuntando a C con
        // epoch_id=22, que es el de C).
        let peeled_b = peel(
            &ms_b,
            &onion.first_key_id,
            &onion.payload,
            &onion.tag,
            2,
            11,
            42,
            1,
            b"hdr",
        )
        .unwrap();
        let inner_b = match peeled_b {
            Peeled::Forward(l) => l,
            _ => panic!("expected Forward"),
        };
        assert_eq!(inner_b.next_orr_id, "orr_c");
        assert_eq!(inner_b.epoch_id, 22);

        // C pela: max_hops=1 ⇒ Forward(InnerLayer apuntando a D con
        // epoch_id=33).
        let peeled_c = peel(
            &ms_c,
            &inner_b.key_id,
            &inner_b.xor_ct,
            &inner_b.tag,
            1,
            22,
            42,
            1,
            b"hdr",
        )
        .unwrap();
        let inner_c = match peeled_c {
            Peeled::Forward(l) => l,
            _ => panic!("expected Forward"),
        };
        assert_eq!(inner_c.next_orr_id, "orr_d");
        assert_eq!(inner_c.epoch_id, 33);

        // D pela: max_hops=0 ⇒ Deliver(body_dkms).
        let peeled_d = peel(
            &ms_d,
            &inner_c.key_id,
            &inner_c.xor_ct,
            &inner_c.tag,
            0,
            33,
            42,
            1,
            b"hdr",
        )
        .unwrap();
        assert_eq!(peeled_d, Peeled::Deliver(body));
    }

    #[test]
    fn build_2_hops_size_growth_is_linear() {
        // body = 64 B, esperado outer ≈ 64 + 45 = ~110 B.
        let ms_x = random_secret();
        let ms_dst = random_secret();
        let body = vec![0xAA; 64];
        let path = vec![
            PathHopSecret {
                orr_id: "orr_x".into(),
                master_secret: Zeroizing::new(ms_x),
                epoch_id: 0,
            },
            PathHopSecret {
                orr_id: "orr_dst".into(),
                master_secret: Zeroizing::new(ms_dst),
                epoch_id: 0,
            },
        ];
        let onion = build_onion(&path, body, 42, 1, b"hdr").unwrap();
        // Tamaño realista: chequeo de cota superior — no debería estar
        // por debajo de 64 ni explotar a > 200 B. Con `epoch_id: u32`
        // añadido a `InnerLayer` (~10 B msgpack-named: 1 B campo,
        // ~8 B nombre "epoch_id", 1-5 B valor según varint), el techo
        // queda holgado en 200 B incluso así.
        assert!(onion.payload.len() >= 64);
        assert!(
            onion.payload.len() < 200,
            "outer payload = {} B (esperado <200)",
            onion.payload.len()
        );
    }

    #[test]
    fn empty_path_fails() {
        let body = vec![1, 2, 3];
        assert!(build_onion(&[], body, 42, 1, b"hdr").is_err());
    }

    #[test]
    fn wrong_secret_is_an_error_not_garbage() {
        // Con el XOR anterior esto devolvía `Deliver(basura)` y el ORR se la
        // entregaba tan tranquilo al DKMS: la divergencia de secretos sólo se
        // notaba más arriba, si acaso. Con AEAD el tag no cuadra y se para aquí.
        let ms = random_secret();
        let wrong = random_secret();
        let body = b"x".to_vec();
        let path = vec![PathHopSecret {
            orr_id: "orr_dst".into(),
            master_secret: Zeroizing::new(ms),
            epoch_id: 0,
        }];
        let onion = build_onion(&path, body, 42, 1, b"hdr").unwrap();
        assert!(peel(
            &wrong,
            &onion.first_key_id,
            &onion.payload,
            &onion.tag,
            0,
            0,
            42,
            1,
            b"hdr"
        )
        .is_err());
    }
}
