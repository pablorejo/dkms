//! Buffer FIFO de claves de transporte para un peer DKMS concreto.
//!
//! Cada `SecureKeyBuffer` corresponde a una dirección concreta:
//!
//! * `buffer_enc[peer=B]` en DKMS_A — claves que A consume al enviar a B.
//! * `buffer_dec[source=A]` en DKMS_B — gemelas de las anteriores, indexadas
//!   por `KeyId` para resolver el `transport_key_id` que viene en el ETSI
//!   020.
//!
//! Invariantes:
//!
//! * El material de la clave vive en `Zeroizing<Vec<u8>>`; al hacer `pop`,
//!   `take_by_id` o al expirar el buffer entero, los bytes se ponen a cero
//!   automáticamente cuando el `Zeroizing` cae.
//! * Una clave es **one-shot**: salir del buffer (por consumo o purga) es
//!   irreversible.
//! * Capacidad fija (`capacity`): `push` aplica backpressure devolviendo el
//!   par `(id, key)` original al *caller*, en vez de tirar la clave o crecer
//!   sin límite.

use std::collections::VecDeque;

use parking_lot::Mutex;
use zeroize::Zeroizing;

use common::ids::KeyId;

/// Una clave de transporte en su forma cruda: `id` + bytes zeroizados.
pub struct TransportKey {
    pub id: KeyId,
    pub bytes: Zeroizing<Vec<u8>>,
}

impl TransportKey {
    pub fn new(id: KeyId, bytes: Vec<u8>) -> Self {
        Self {
            id,
            bytes: Zeroizing::new(bytes),
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl std::fmt::Debug for TransportKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransportKey")
            .field("id", &self.id)
            .field("len", &self.bytes.len())
            .finish()
    }
}

pub struct SecureKeyBuffer {
    capacity: usize,
    inner: Mutex<VecDeque<TransportKey>>,
}

impl SecureKeyBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(4096))),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }

    /// True si por debajo del watermark merece la pena disparar refill.
    pub fn below_watermark(&self, watermark: usize) -> bool {
        self.len() < watermark
    }

    /// Inserta una clave de transporte. El `capacity` es **soft-hint**
    /// para el classify de QoS (fill_ratio); el push real nunca rechaza
    /// ni descarta. El consumo de RAM se acota porque tanto el generator
    /// (productor de `enc`) como el receptor ORR (productor de `dec`)
    /// están limitados por la rate del SDN y el `max_in_flight`, y los
    /// SAEs van drenando vía `pop_oldest` / `take_by_id`.
    ///
    /// El tipo de retorno sigue siendo `Result<…, TransportKey>` para no
    /// romper callers existentes; ahora siempre devuelve `Ok(())`.
    pub fn try_push(&self, key: TransportKey) -> std::result::Result<(), TransportKey> {
        self.inner.lock().push_back(key);
        Ok(())
    }

    /// Como `try_push` pero rechaza (devuelve la clave) si el buffer ya tiene
    /// `ceiling` o más elementos (B6). Se usa en el camino DEC con un techo muy
    /// por encima del backlog honesto: el receptor acusa AL RECIBIR (no al
    /// drenar), así que `dec` honesto puede superar `capacity` con SAEs lentos
    /// — cortar en `capacity` reabriría el deadlock medido en 2026-05. El techo
    /// solo lo alcanza un flood.
    pub fn try_push_capped(
        &self,
        key: TransportKey,
        ceiling: usize,
    ) -> std::result::Result<(), TransportKey> {
        let mut q = self.inner.lock();
        if q.len() >= ceiling {
            return Err(key);
        }
        q.push_back(key);
        Ok(())
    }

    /// Consume **la siguiente** clave en orden FIFO (rol ENC).
    pub fn pop_oldest(&self) -> Option<TransportKey> {
        self.inner.lock().pop_front()
    }

    /// Consume **una clave específica por su `KeyId`** (rol DEC). El DKMS
    /// destino la usa al recibir un ETSI 020 que trae `transport_key_id` en
    /// `extension`.
    pub fn take_by_id(&self, id: &KeyId) -> Option<TransportKey> {
        let mut q = self.inner.lock();
        let pos = q.iter().position(|k| &k.id == id)?;
        q.remove(pos)
    }

    /// Borra todo el contenido (los `Zeroizing` se zeroizan al ser drop).
    pub fn clear(&self) {
        self.inner.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(id: &str, n: usize) -> TransportKey {
        TransportKey::new(KeyId::new(id), vec![0xAB; n])
    }

    #[test]
    fn push_and_pop_are_fifo() {
        let b = SecureKeyBuffer::new(4);
        b.try_push(k("a", 32)).unwrap();
        b.try_push(k("b", 32)).unwrap();
        b.try_push(k("c", 32)).unwrap();
        assert_eq!(b.len(), 3);

        assert_eq!(b.pop_oldest().unwrap().id.as_str(), "a");
        assert_eq!(b.pop_oldest().unwrap().id.as_str(), "b");
        assert_eq!(b.pop_oldest().unwrap().id.as_str(), "c");
        assert!(b.pop_oldest().is_none());
    }

    #[test]
    fn take_by_id_removes_in_place() {
        let b = SecureKeyBuffer::new(8);
        b.try_push(k("a", 32)).unwrap();
        b.try_push(k("b", 32)).unwrap();
        b.try_push(k("c", 32)).unwrap();
        let got = b.take_by_id(&KeyId::new("b")).expect("present");
        assert_eq!(got.id.as_str(), "b");
        assert_eq!(b.len(), 2);
        // a y c siguen en orden
        assert_eq!(b.pop_oldest().unwrap().id.as_str(), "a");
        assert_eq!(b.pop_oldest().unwrap().id.as_str(), "c");
    }

    #[test]
    fn try_push_capped_rejects_at_ceiling() {
        let b = SecureKeyBuffer::new(4);
        assert!(b.try_push_capped(k("a", 32), 2).is_ok());
        assert!(b.try_push_capped(k("b", 32), 2).is_ok());
        // Al alcanzar el techo, rechaza y devuelve la clave (B6).
        assert!(b.try_push_capped(k("c", 32), 2).is_err());
        assert_eq!(b.len(), 2);
    }

    #[test]
    fn take_by_id_misses_when_absent() {
        let b = SecureKeyBuffer::new(8);
        b.try_push(k("a", 32)).unwrap();
        assert!(b.take_by_id(&KeyId::new("z")).is_none());
        assert_eq!(b.len(), 1);
    }
}
