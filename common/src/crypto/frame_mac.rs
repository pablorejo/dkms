//! MAC de los frames de datos del enlace QKC↔QKC — integridad, autenticación
//! de origen y frescura en una sola pieza.
//!
//! ## Por qué
//!
//! El payload va cifrado con OTP (`qkc/src/crypto.rs`), que da confidencialidad
//! perfecta y **cero integridad**: XOR es maleable, así que quien esté en medio
//! puede aplicar cualquier delta al ciphertext y el receptor descifra un
//! plaintext modificado sin enterarse. El `key_digest` del DKMS es un SHA-256
//! sin clave que viaja dentro del cifrado: apoya la integridad en la
//! confidencialidad y sólo cubre el camino del `DKMS_BUFFER`. Y `frame.sender_id`
//! es un `u32` en claro que nadie verifica, así que tampoco hay autenticación
//! de origen a nivel de frame.
//!
//! Este módulo cierra las tres cosas a la vez. HMAC-SHA256 con clave de 256 bits
//! es quantum-safe (Grover deja 128 bits efectivos); la raíz del enlace viene de
//! material PQC o QKD, nunca de algo que un adversario cuántico pueda romper.
//!
//! ## Salto a salto, no extremo a extremo
//!
//! El QKC descifra y recifra en cada salto (`relay.rs`), así que el MAC se
//! recalcula por enlace, igual que el OTP. Un QKC intermedio **malicioso** sigue
//! pudiendo alterar el contenido: eso no lo cubre esta capa, sino el `key_digest`
//! del DKMS extremo a extremo. Lo que se cierra aquí es el atacante *en el cable*
//! entre dos QKCs, que hasta ahora podía modificar y reinyectar a voluntad.
//!
//! ## Frescura
//!
//! La lección del NOTIFY (que se autenticó con `epoch = 0` fijo y por tanto era
//! reinyectable) es que la frescura tiene que estar en el mensaje autenticado
//! **desde el diseño**. Aquí van dos campos dentro del MAC:
//!
//! * `session` — la encarnación del emisor para este enlace, aleatoria por
//!   arranque de proceso. Mismo patrón que el `incarnation` del DKMS. Sin ella,
//!   un emisor que reinicia vuelve al contador 1 y sus frames legítimos serían
//!   indistinguibles de un replay.
//! * `counter` — monotónico dentro de la sesión, empieza en 1. El receptor lleva
//!   una [`ReplayWindow`] deslizante.
//!
//! Ambos entran en la clave derivada *y* en el mensaje: cambiar de sesión cambia
//! la clave, así que un frame de una sesión no verifica jamás en otra.
//!
//! ## Layout del mensaje canónico
//!
//! ```text
//!   TAG_FRAME || u8(kind) || u8(grade) || u64_be(session) || u64_be(counter)
//!             || u32_be(sender_id) || u32_be(receiver_id) || u32_be(dest_final)
//!             || u16_be(key_size_bits) || u32_be(epoch_id)
//!             || u32_be(n_key_ids) || lp16(key_id)*        // en orden
//!             || lp16(header_orr_mp) || lp16(header_dkms_mp)
//!             || u32_be(body.len()) || body                // ciphertext sin trailer
//!   lp16(x) := u16_be(x.len()) || x
//! ```
//!
//! Todo el frame entra: `sender_id` ata la identidad, `dest_final` y los headers
//! impiden que un atacante redirija el frame, y el `body` es la integridad del
//! ciphertext. Los `key_ids` van con longitud explícita para que no se pueda
//! mover el límite entre dos campos contiguos.

use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Etiqueta de dominio de los frames de datos (`FRAME_RECV_AUTH`/`FRAME_RELAY_AUTH`).
/// Distinta de las de `link_mac` para que un tag de handshake no valga como tag
/// de datos ni al revés.
pub const TAG_FRAME: &[u8] = b"QKCFRAME";

/// `info` del HKDF que deriva la clave de frames desde la raíz del enlace.
const HKDF_INFO: &[u8] = b"dkms/link-mac/frame/v1";

/// Longitud del tag HMAC-SHA256.
pub const TAG_LEN: usize = 32;

/// Tamaño por defecto de la ventana anti-replay, en frames.
///
/// La recepción en un socket TCP está ordenada, pero el emisor asigna contadores
/// desde varias tareas concurrentes (`max_emits_in_flight` llega a 128 por
/// defecto), así que el orden de numeración y el de escritura no coinciden.
/// 1024 deja margen de sobra sin que el bitmap pase de 128 B.
pub const DEFAULT_WINDOW: u64 = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MacError {
    #[error("invalid frame MAC")]
    Invalid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// El contador ya se había visto en esta sesión.
    #[error("replayed frame (session {session}, counter {counter})")]
    Replayed { session: u64, counter: u64 },
    /// El contador queda por detrás de la ventana: o es muy viejo, o el emisor
    /// dio un salto enorme y luego mandó los rezagados.
    #[error("frame too old (session {session}, counter {counter}, window ends at {floor})")]
    TooOld {
        session: u64,
        counter: u64,
        floor: u64,
    },
    /// Sesión ya retirada: el emisor la abandonó al reiniciar y alguien intenta
    /// revivirla reinyectando frames viejos.
    #[error("retired session {session}")]
    RetiredSession { session: u64 },
    /// Los contadores empiezan en 1; el 0 no es válido.
    #[error("zero counter is not valid")]
    ZeroCounter,
}

/// Deriva la clave del MAC de frames a partir de la raíz del enlace.
///
/// La raíz es, por orden de preferencia:
///   1. el secreto compartido del handshake ML-KEM del enlace (enlaces PQC —
///      rota con el relink y no necesita configuración), o
///   2. el `link_psk` configurado localmente (enlaces QKD, que no tienen
///      handshake propio entre QKCs).
///
/// La `session` entra como *salt*, así que dos encarnaciones del mismo emisor
/// tienen claves distintas y un frame de la sesión vieja no verifica en la nueva
/// ni aunque el contador coincida.
pub fn derive_key(root: &[u8], session: u64) -> [u8; 32] {
    let hk = Hkdf::<Sha256>::new(Some(&session.to_be_bytes()), root);
    let mut okm = [0u8; 32];
    hk.expand(HKDF_INFO, &mut okm)
        .expect("32 bytes is a valid HKDF-SHA256 output length");
    okm
}

/// Los campos del frame que entran en el MAC.
///
/// `body` es el ciphertext **sin** el trailer de autenticación: el trailer lleva
/// el propio tag, así que no puede cubrirse a sí mismo.
#[derive(Debug, Clone, Copy)]
pub struct FrameAad<'a> {
    pub kind: u8,
    pub grade: u8,
    pub session: u64,
    pub counter: u64,
    pub sender_id: u32,
    pub receiver_id: u32,
    pub dest_final: u32,
    pub key_size_bits: u16,
    pub epoch_id: u32,
    pub key_ids: &'a [String],
    pub header_orr_mp: &'a [u8],
    pub header_dkms_mp: &'a [u8],
    pub body: &'a [u8],
}

fn write_lp16(mac: &mut HmacSha256, x: &[u8]) {
    // assert duro, no debug_assert: en release el cast truncaría y la
    // codificación con prefijos dejaría de ser inyectiva (dos campos vecinos
    // podrían mover su frontera). Inalcanzable desde el wire (el decoder
    // acota a u16) y desde productores sanos (`Frame::validate_for_encode`);
    // defensa en profundidad.
    assert!(x.len() <= u16::MAX as usize, "lp16 overflow: {}", x.len());
    mac.update(&(x.len() as u16).to_be_bytes());
    mac.update(x);
}

fn build(key: &[u8; 32], aad: &FrameAad<'_>) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(TAG_FRAME);
    mac.update(&[aad.kind, aad.grade]);
    mac.update(&aad.session.to_be_bytes());
    mac.update(&aad.counter.to_be_bytes());
    mac.update(&aad.sender_id.to_be_bytes());
    mac.update(&aad.receiver_id.to_be_bytes());
    mac.update(&aad.dest_final.to_be_bytes());
    mac.update(&aad.key_size_bits.to_be_bytes());
    mac.update(&aad.epoch_id.to_be_bytes());
    mac.update(&(aad.key_ids.len() as u32).to_be_bytes());
    for k in aad.key_ids {
        write_lp16(&mut mac, k.as_bytes());
    }
    write_lp16(&mut mac, aad.header_orr_mp);
    write_lp16(&mut mac, aad.header_dkms_mp);
    mac.update(&(aad.body.len() as u32).to_be_bytes());
    mac.update(aad.body);
    mac
}

/// Calcula el tag de un frame.
pub fn tag(key: &[u8; 32], aad: &FrameAad<'_>) -> [u8; TAG_LEN] {
    let out = build(key, aad).finalize().into_bytes();
    let mut t = [0u8; TAG_LEN];
    t.copy_from_slice(&out);
    t
}

/// Verifica el tag de un frame (constant-time vía `Hmac::verify_slice`).
pub fn verify(key: &[u8; 32], aad: &FrameAad<'_>, mac: &[u8]) -> Result<(), MacError> {
    build(key, aad)
        .verify_slice(mac)
        .map_err(|_| MacError::Invalid)
}

/// Ventana deslizante anti-replay para un enlace y un sentido.
///
/// **Orden de uso obligatorio: verificar el MAC primero, y sólo después llamar a
/// [`ReplayWindow::check_and_set`].** Al revés, cualquiera podría rotar nuestra
/// sesión —y por tanto tirar la ventana entera— mandando basura con un `session`
/// inventado. Con el MAC delante, para llegar aquí hace falta la clave del enlace.
#[derive(Debug)]
pub struct ReplayWindow {
    width: u64,
    /// Sesión en curso. `None` mientras no haya llegado ningún frame válido.
    session: Option<u64>,
    /// Contador más alto aceptado en la sesión en curso.
    highest: u64,
    /// Bitmap de los `width` contadores que terminan en `highest`. El bit `i`
    /// corresponde a `highest - i`.
    bits: Vec<u64>,
    /// Sesiones abandonadas, para que nadie las reviva con frames capturados.
    retired: Vec<u64>,
    /// Ventana de la sesión ANTERIOR: un frame sellado justo antes de una
    /// rotación puede llegar después del primero de la nueva (contadores
    /// concurrentes, orden de escritura ≠ orden de sellado). Se acepta si su
    /// contador es fresco AHÍ; sin esto, o se perdía o —peor— tiraba la
    /// sesión nueva (auditoría 2026-09-03, C-08).
    prev: Option<(u64, u64, Vec<u64>)>,
    /// Cómo se adopta una sesión nueva (ver [`SessionAdopt`]).
    adopt: SessionAdopt,
    /// Último frame aceptado en la sesión actual (modo no monótono: una
    /// sesión nueva solo se adopta si la actual lleva `quiet` sin verificar
    /// nada — un peer reiniciado calla, un forjador no).
    last_accept: Option<std::time::Instant>,
    /// Silencio exigido en `AfterQuiet` (default `ADOPT_AFTER_QUIET`).
    quiet: std::time::Duration,
}

/// Modo `AfterQuiet`: silencio mínimo de la sesión actual para adoptar otra.
const ADOPT_AFTER_QUIET: std::time::Duration = std::time::Duration::from_secs(10);

/// Cuándo un frame verificado con una sesión distinta de la actual la
/// convierte en la nueva sesión (C-08). Las sesiones ya vistas (la anterior,
/// las retiradas) nunca se readoptan, en ninguna política.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAdopt {
    /// Cualquier sesión no vista se adopta en el acto. Para claves EFÍMERAS
    /// por proceso (e2e del DKMS, cebolla del ORR): un frame capturado de una
    /// sesión anterior a nuestro reinicio ya no verifica, así que lo único
    /// que hay que impedir es revivir las sesiones vistas.
    Any,
    /// Solo una sesión MAYOR (épocas): una menor es un frame viejo.
    Monotonic,
    /// Solo si la actual lleva `ADOPT_AFTER_QUIET` sin verificar nada. Para
    /// una raíz FIJA (PSK): la clave de cualquier sesión se deriva igual
    /// antes y después de un reinicio nuestro, así que un frame capturado de
    /// una sesión que este proceso no vio verificaría y, adoptado, dejaba
    /// muda la sesión viva. Un peer reiniciado de verdad calla.
    AfterQuiet,
}

/// Cuántas sesiones retiradas se recuerdan. Un enlace que ve más reinicios que
/// esto en la vida del proceso olvida los más viejos; el riesgo residual es
/// aceptar un replay de una sesión muy antigua, cuya clave además ya rotó.
const RETIRED_KEPT: usize = 8;

impl ReplayWindow {
    pub fn new(width: u64) -> Self {
        let width = width.max(64);
        Self {
            width,
            session: None,
            highest: 0,
            bits: vec![0u64; (width as usize).div_ceil(64)],
            retired: Vec::new(),
            prev: None,
            adopt: SessionAdopt::Any,
            last_accept: None,
            quiet: ADOPT_AFTER_QUIET,
        }
    }

    /// Silencio exigido antes de adoptar otra sesión en `AfterQuiet`.
    /// Para tests (un reinicio «inmediato» no puede esperar 10 s).
    pub fn set_quiet(&mut self, quiet: std::time::Duration) {
        self.quiet = quiet;
    }

    /// Ventana cuyas sesiones son números de ÉPOCA (crecientes): una sesión
    /// menor que la actual es un frame viejo, nunca un reinicio.
    pub fn new_monotonic(width: u64) -> Self {
        let mut w = Self::new(width);
        w.adopt = SessionAdopt::Monotonic;
        w
    }

    /// Ventana para una raíz FIJA (PSK): una sesión nueva solo se adopta
    /// cuando la actual lleva un rato callada (ver [`SessionAdopt`]).
    pub fn new_quiet_gated(width: u64) -> Self {
        let mut w = Self::new(width);
        w.adopt = SessionAdopt::AfterQuiet;
        w
    }

    /// Sesión que la ventana está seguiendo ahora mismo.
    pub fn session(&self) -> Option<u64> {
        self.session
    }

    /// Contador más alto aceptado en la sesión en curso.
    pub fn highest(&self) -> u64 {
        self.highest
    }

    fn reset_to(&mut self, session: u64) {
        if let Some(old) = self.session {
            if old != session {
                // La anterior queda viva un turno (frames tardíos); la que
                // había en `prev` sí se retira del todo.
                if let Some((p, _, _)) = self.prev.take() {
                    self.retired.push(p);
                    if self.retired.len() > RETIRED_KEPT {
                        self.retired.remove(0);
                    }
                }
                self.prev = Some((old, self.highest, self.bits.clone()));
            }
        }
        self.session = Some(session);
        self.highest = 0;
        self.bits.iter_mut().for_each(|w| *w = 0);
    }

    /// Frame de la sesión anterior: fresco si su contador no está en la
    /// ventana guardada de esa sesión (y no la reabre por encima).
    fn check_and_set_prev(&mut self, session: u64, counter: u64) -> Result<(), ReplayError> {
        let width = self.width;
        let Some((p, highest, bits)) = self.prev.as_mut() else {
            return Err(ReplayError::RetiredSession { session });
        };
        debug_assert_eq!(*p, session);
        if counter > *highest {
            // Por encima de lo visto en esa sesión: el emisor siguió
            // contando antes de rotar; se acepta y se avanza la ventana.
            let by = counter - *highest;
            if by >= width {
                bits.iter_mut().for_each(|w| *w = 0);
            } else {
                shift_bits(bits, by);
            }
            *highest = counter;
            bits[0] |= 1;
            return Ok(());
        }
        let back = *highest - counter;
        if back >= width {
            return Err(ReplayError::TooOld {
                session,
                counter,
                floor: highest.saturating_sub(width - 1),
            });
        }
        let i = back as usize;
        if bits[i / 64] & (1u64 << (i % 64)) != 0 {
            return Err(ReplayError::Replayed { session, counter });
        }
        bits[i / 64] |= 1u64 << (i % 64);
        Ok(())
    }

    fn is_set(&self, back: u64) -> bool {
        let i = back as usize;
        self.bits[i / 64] & (1u64 << (i % 64)) != 0
    }

    fn set(&mut self, back: u64) {
        let i = back as usize;
        self.bits[i / 64] |= 1u64 << (i % 64);
    }

    fn shift(&mut self, by: u64) {
        if by >= self.width {
            self.bits.iter_mut().for_each(|w| *w = 0);
            return;
        }
        shift_bits(&mut self.bits, by);
    }

    /// Acepta `(session, counter)` si es fresco, y lo marca como visto.
    ///
    /// Llamar **sólo tras haber verificado el MAC** (ver la nota del tipo).
    pub fn check_and_set(&mut self, session: u64, counter: u64) -> Result<(), ReplayError> {
        if counter == 0 {
            return Err(ReplayError::ZeroCounter);
        }
        if self.retired.contains(&session) {
            return Err(ReplayError::RetiredSession { session });
        }
        if let Some((p, _, _)) = &self.prev {
            if *p == session {
                return self.check_and_set_prev(session, counter);
            }
        }
        if let Some(cur) = self.session {
            if cur != session {
                // ¿Adoptar la sesión nueva? Antes se adoptaba SIEMPRE, y la
                // actual se retiraba: un frame CAPTURADO de una sesión vieja
                // (verifica: su clave sigue en la historia) enmudecía el
                // enlace hasta la siguiente rotación (C-08).
                let adopt = match self.adopt {
                    SessionAdopt::Any => true,
                    SessionAdopt::Monotonic => session > cur,
                    SessionAdopt::AfterQuiet => {
                        self.last_accept.is_none_or(|t| t.elapsed() >= self.quiet)
                    }
                };
                if !adopt {
                    return Err(ReplayError::RetiredSession { session });
                }
                self.reset_to(session);
            }
        } else {
            self.reset_to(session);
        }
        self.last_accept = Some(std::time::Instant::now());
        if counter > self.highest {
            let by = counter - self.highest;
            self.shift(by);
            self.highest = counter;
            self.set(0);
            return Ok(());
        }
        let back = self.highest - counter;
        if back >= self.width {
            return Err(ReplayError::TooOld {
                session,
                counter,
                floor: self.highest.saturating_sub(self.width - 1),
            });
        }
        if self.is_set(back) {
            return Err(ReplayError::Replayed { session, counter });
        }
        self.set(back);
        Ok(())
    }
}

fn shift_bits(bits: &mut [u64], by: u64) {
    let words = (by / 64) as usize;
    let rem = (by % 64) as u32;
    let n = bits.len();
    if words > 0 {
        for i in (0..n).rev() {
            bits[i] = if i >= words { bits[i - words] } else { 0 };
        }
    }
    if rem > 0 {
        let mut carry = 0u64;
        for w in bits.iter_mut() {
            let next = *w >> (64 - rem);
            *w = (*w << rem) | carry;
            carry = next;
        }
    }
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new(DEFAULT_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &[u8] = b"link root secret, 32+ bytes of it!!";

    fn aad<'a>(counter: u64, body: &'a [u8], key_ids: &'a [String]) -> FrameAad<'a> {
        FrameAad {
            kind: 0x04,
            grade: 0,
            session: 7,
            counter,
            sender_id: 1,
            receiver_id: 2,
            dest_final: 9,
            key_size_bits: 256,
            epoch_id: 3,
            key_ids,
            header_orr_mp: b"orr",
            header_dkms_mp: b"dkms",
            body,
        }
    }

    #[test]
    fn round_trip() {
        let k = derive_key(ROOT, 7);
        let ids = vec!["id-1".to_string()];
        let a = aad(1, b"ciphertext", &ids);
        let t = tag(&k, &a);
        assert_eq!(t.len(), TAG_LEN);
        assert!(verify(&k, &a, &t).is_ok());
    }

    #[test]
    fn every_field_is_bound() {
        let k = derive_key(ROOT, 7);
        let ids = vec!["id-1".to_string()];
        let base = aad(1, b"ciphertext", &ids);
        let t = tag(&k, &base);

        // Modificar el ciphertext: es EL caso que hoy pasa desapercibido con OTP.
        let mut body2 = b"ciphertext".to_vec();
        body2[0] ^= 0xFF;
        let mut a = base;
        a.body = &body2;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Suplantar el emisor.
        let mut a = base;
        a.sender_id = 99;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Redirigir el frame.
        let mut a = base;
        a.dest_final = 42;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Tocar los headers que el QKC propaga sin mirar.
        let mut a = base;
        a.header_dkms_mp = b"dkms!";
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Cambiar el grade (invariante "una clave QKD nunca cruza un enlace PQC").
        let mut a = base;
        a.grade = 1;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Cambiar el contador o la sesión.
        let mut a = base;
        a.counter = 2;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));
        let mut a = base;
        a.session = 8;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));

        // Cambiar los key_ids (el receptor los usa para elegir claves DEC).
        let ids2 = vec!["id-2".to_string()];
        let mut a = base;
        a.key_ids = &ids2;
        assert_eq!(verify(&k, &a, &t), Err(MacError::Invalid));
    }

    #[test]
    fn key_ids_boundary_cannot_slide() {
        // Sin longitudes explícitas, ["ab","c"] y ["a","bc"] darían el mismo
        // mensaje canónico. lp16 por elemento lo impide.
        let k = derive_key(ROOT, 7);
        let a_ids = vec!["ab".to_string(), "c".to_string()];
        let b_ids = vec!["a".to_string(), "bc".to_string()];
        let a = aad(1, b"x", &a_ids);
        let t = tag(&k, &a);
        let mut b = a;
        b.key_ids = &b_ids;
        assert_eq!(verify(&k, &b, &t), Err(MacError::Invalid));
    }

    #[test]
    fn different_sessions_derive_different_keys() {
        assert_ne!(derive_key(ROOT, 1), derive_key(ROOT, 2));
        assert_ne!(derive_key(ROOT, 1), derive_key(b"other root", 1));
    }

    #[test]
    fn wrong_root_rejected() {
        let ids: Vec<String> = vec![];
        let a = aad(1, b"x", &ids);
        let t = tag(&derive_key(ROOT, 7), &a);
        assert_eq!(
            verify(&derive_key(b"wrong root", 7), &a, &t),
            Err(MacError::Invalid)
        );
    }

    #[test]
    fn handshake_tag_is_not_a_frame_tag() {
        // Cross-protocol: el tag de `link_mac` (TAG_INIT) no debe valer aquí.
        let ids: Vec<String> = vec![];
        let a = aad(1, b"blob", &ids);
        let k = derive_key(ROOT, 7);
        let handshake = crate::crypto::link_mac::tag(
            &k,
            crate::crypto::link_mac::TAG_INIT,
            3,
            1,
            2,
            b"blob",
            "ml-kem-768",
            256,
        );
        assert_eq!(verify(&k, &a, &handshake), Err(MacError::Invalid));
    }

    #[test]
    fn wrong_length_mac_rejected_cleanly() {
        let ids: Vec<String> = vec![];
        let a = aad(1, b"x", &ids);
        let k = derive_key(ROOT, 7);
        assert_eq!(verify(&k, &a, &[0u8; 16]), Err(MacError::Invalid));
        assert_eq!(verify(&k, &a, &[]), Err(MacError::Invalid));
    }

    // ─── ventana anti-replay ────────────────────────────────────────

    #[test]
    fn window_accepts_in_order_and_rejects_repeat() {
        let mut w = ReplayWindow::new(64);
        for c in 1..=100 {
            assert!(w.check_and_set(1, c).is_ok(), "counter {c}");
        }
        assert_eq!(
            w.check_and_set(1, 100),
            Err(ReplayError::Replayed {
                session: 1,
                counter: 100
            })
        );
        assert_eq!(
            w.check_and_set(1, 80),
            Err(ReplayError::Replayed {
                session: 1,
                counter: 80
            })
        );
    }

    #[test]
    fn window_accepts_out_of_order_within_width() {
        let mut w = ReplayWindow::new(64);
        assert!(w.check_and_set(1, 10).is_ok());
        // Los rezagados 1..9 siguen entrando: el emisor numera desde varias
        // tareas, así que llegar desordenado es normal.
        for c in 1..10 {
            assert!(w.check_and_set(1, c).is_ok(), "late {c}");
        }
        // Pero sólo una vez cada uno.
        assert!(w.check_and_set(1, 5).is_err());
    }

    #[test]
    fn window_rejects_beyond_width() {
        let mut w = ReplayWindow::new(64);
        assert!(w.check_and_set(1, 200).is_ok());
        // 200 - 64 = 136 y hacia atrás queda fuera.
        assert!(matches!(
            w.check_and_set(1, 100),
            Err(ReplayError::TooOld { .. })
        ));
        assert!(w.check_and_set(1, 199).is_ok());
    }

    #[test]
    fn window_survives_a_big_jump() {
        let mut w = ReplayWindow::new(128);
        assert!(w.check_and_set(1, 1).is_ok());
        assert!(w.check_and_set(1, 1_000_000).is_ok());
        // El salto vacía la ventana; el 1 queda muy por detrás.
        assert!(matches!(
            w.check_and_set(1, 1),
            Err(ReplayError::TooOld { .. })
        ));
        assert!(w.check_and_set(1, 999_999).is_ok());
    }

    #[test]
    fn new_session_resets_the_window() {
        // Monótona (épocas): una sesión mayor se adopta en el acto.
        let mut w = ReplayWindow::new_monotonic(64);
        assert!(w.check_and_set(1, 50).is_ok());
        assert!(w.check_and_set(2, 1).is_ok());
        assert_eq!(w.session(), Some(2));
        assert_eq!(w.highest(), 1);
    }

    #[test]
    fn retired_session_cannot_be_revived() {
        let mut w = ReplayWindow::new_monotonic(64);
        assert!(w.check_and_set(1, 5).is_ok());
        assert!(w.check_and_set(2, 1).is_ok());
        assert!(w.check_and_set(3, 1).is_ok());
        // La sesión 1 ya está dos rotaciones atrás: retirada del todo.
        assert_eq!(
            w.check_and_set(1, 5),
            Err(ReplayError::RetiredSession { session: 1 })
        );
        assert_eq!(
            w.check_and_set(1, 6),
            Err(ReplayError::RetiredSession { session: 1 })
        );
        // Y la actual sigue viva: el capturado no la tiró (C-08).
        assert!(w.check_and_set(3, 2).is_ok());
    }

    #[test]
    fn a_captured_frame_of_an_older_session_does_not_mute_the_current_one() {
        // C-08: antes, cualquier sesión distinta se adoptaba y la actual se
        // retiraba: un frame capturado de la sesión 1 dejaba mudo al enlace.
        let mut w = ReplayWindow::new_monotonic(64);
        assert!(w.check_and_set(5, 10).is_ok());
        assert_eq!(
            w.check_and_set(1, 7),
            Err(ReplayError::RetiredSession { session: 1 })
        );
        assert_eq!(w.session(), Some(5));
        assert!(w.check_and_set(5, 11).is_ok());
        // Raíz fija (PSK): tampoco, mientras la actual esté viva.
        let mut w = ReplayWindow::new_quiet_gated(64);
        assert!(w.check_and_set(77, 3).is_ok());
        assert_eq!(
            w.check_and_set(12, 1),
            Err(ReplayError::RetiredSession { session: 12 })
        );
        assert!(w.check_and_set(77, 4).is_ok());
        // Claves efímeras (`Any`): una sesión no vista se adopta en el acto,
        // pero la anterior y las retiradas no vuelven.
        let mut w = ReplayWindow::new(64);
        assert!(w.check_and_set(9, 1).is_ok());
        assert!(w.check_and_set(4, 1).is_ok());
        assert_eq!(w.session(), Some(4));
        assert!(
            w.check_and_set(9, 2).is_ok(),
            "la anterior sigue aceptando frescos"
        );
        assert!(w.check_and_set(1, 1).is_ok());
        assert_eq!(
            w.check_and_set(9, 3),
            Err(ReplayError::RetiredSession { session: 9 })
        );
    }

    #[test]
    fn late_frames_of_the_previous_session_are_still_accepted_once() {
        let mut w = ReplayWindow::new_monotonic(64);
        assert!(w.check_and_set(1, 5).is_ok());
        assert!(w.check_and_set(2, 1).is_ok());
        // Sellado antes de rotar, llegado después: fresco en su ventana.
        assert!(w.check_and_set(1, 6).is_ok());
        assert!(w.check_and_set(1, 4).is_ok());
        // Pero solo una vez.
        assert_eq!(
            w.check_and_set(1, 6),
            Err(ReplayError::Replayed {
                session: 1,
                counter: 6
            })
        );
        assert_eq!(w.session(), Some(2));
    }

    #[test]
    fn zero_counter_rejected() {
        let mut w = ReplayWindow::new(64);
        assert_eq!(w.check_and_set(1, 0), Err(ReplayError::ZeroCounter));
    }

    #[test]
    fn window_width_has_a_floor() {
        // Pedir una ventana ridícula no debe dejar el bitmap vacío.
        let mut w = ReplayWindow::new(1);
        assert!(w.check_and_set(1, 1).is_ok());
        assert!(w.check_and_set(1, 2).is_ok());
        assert!(w.check_and_set(1, 1).is_err());
    }

    #[test]
    fn full_flow_forged_frame_never_reaches_the_window() {
        // El orden importa: MAC primero, ventana después. Un frame forjado con
        // una sesión inventada no debe poder rotar la ventana del receptor.
        let k = derive_key(ROOT, 7);
        let ids: Vec<String> = vec![];
        let mut w = ReplayWindow::new(64);

        let good = aad(1, b"real", &ids);
        let t = tag(&k, &good);
        assert!(verify(&k, &good, &t).is_ok());
        assert!(w.check_and_set(good.session, good.counter).is_ok());

        // Atacante: sesión nueva, contador 1, tag basura.
        let mut forged = good;
        forged.session = 999;
        assert_eq!(verify(&k, &forged, &[0u8; TAG_LEN]), Err(MacError::Invalid));
        // Al fallar el MAC, el caller no llega a tocar la ventana.
        assert_eq!(w.session(), Some(7));
    }
}
