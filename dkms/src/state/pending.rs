//! `PendingStore` — claves de sesión *K* a la espera de ser consumidas por
//! los SAEs autorizados.
//!
//! Una entrada vive aquí desde que:
//!
//! * DKMS_A genera *K* tras un ETSI 014 `enc_keys` (lo guarda para los
//!   SAEs locales que pertenezcan a `target_sae_ids`), **o**
//! * DKMS_B recibe un ETSI 020 con *K* envuelta y la deja accesible para
//!   los `target_sae_ids` de su lado.
//!
//! Política:
//!
//! * Una entrada se borra cuando **todos** los SAEs autorizados la han
//!   recuperado o cuando vence el TTL.
//! * Un SAE concreto solo puede recuperar **una vez** la misma `key_id`
//!   (idempotencia + protección contra abuso).
//! * Si llega un SAE no autorizado, devolvemos `KeyNotAuthorized` (el
//!   handler convierte a 404 para no filtrar la existencia).

/// Cota dura del TTL de una entrada de `PendingStore`. Un `ttl_seconds`
/// elegido por el peer no puede empujar `expires_at` al overflow (B1).
const MAX_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// Entradas vivas que un mismo peer puede tener aquí (auditoría 2026-09-03,
/// B-02/R4). Por encima, sus `ext_keys` se rechazan hasta que sus SAEs
/// recojan o expiren. Muy por encima de lo honesto: a 320 claves/s por par
/// y 5 min de TTL sin que ningún SAE recoja son ~96k.
pub const MAX_PENDING_PER_ORIGIN: usize = 131_072;

/// Por qué no se pudo insertar una clave que viene de un peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingInsertError {
    /// Ya hay una entrada con ese `key_id` (B-01): nunca se pisa.
    Occupied,
    /// El origen ha alcanzado [`MAX_PENDING_PER_ORIGIN`].
    OriginFull,
}

use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};

use dashmap::DashMap;
use tracing::{debug, trace};
use zeroize::Zeroizing;

use common::ids::{KeyId, SaeId};

use crate::error::{DkmsError, Result};

/// Una clave de sesión pendiente de ser recuperada por sus SAEs autorizados.
pub struct PendingEntry {
    /// SAE que originó la generación (master). Necesario para construir el
    /// `source_KME_ID` / `master_SAE_ID` en la respuesta ETSI 014.
    pub initiator: SaeId,
    /// SAEs que pueden recuperar esta clave. Es la *unión* del master y
    /// `additional_slave_SAE_IDs` (multicast).
    pub authorized: HashSet<SaeId>,
    /// SAEs que ya la consumieron (one-shot por SAE).
    pub retrieved: HashSet<SaeId>,
    /// Material de la clave; se zeroiza al hacer drop de la entrada.
    pub material: Zeroizing<Vec<u8>>,
    /// Vencimiento absoluto.
    pub expires_at: Instant,
    /// Peer DKMS del que vino (None = la generó este nodo). Para la cota
    /// por origen.
    pub origin: Option<String>,
}

impl PendingEntry {
    fn is_expired(&self, now: Instant) -> bool {
        now >= self.expires_at
    }

    fn is_complete(&self) -> bool {
        self.retrieved.is_superset(&self.authorized)
    }
}

pub struct PendingStore {
    inner: DashMap<KeyId, PendingEntry>,
    default_ttl: Duration,
    /// Entradas vivas por peer de origen.
    per_origin: DashMap<String, usize>,
}

impl PendingStore {
    pub fn new(default_ttl_secs: u64) -> Self {
        Self {
            inner: DashMap::new(),
            default_ttl: Duration::from_secs(default_ttl_secs),
            per_origin: DashMap::new(),
        }
    }

    fn note_removed(&self, entry: &PendingEntry) {
        if let Some(o) = &entry.origin {
            if let Some(mut c) = self.per_origin.get_mut(o) {
                *c = c.saturating_sub(1);
            }
        }
    }

    fn remove_entry(&self, key_id: &KeyId) -> bool {
        match self.inner.remove(key_id) {
            Some((_, e)) => {
                self.note_removed(&e);
                true
            }
            None => false,
        }
    }

    /// Entradas vivas de un peer de origen.
    pub fn pending_from(&self, origin: &str) -> usize {
        self.per_origin.get(origin).map(|c| *c).unwrap_or(0)
    }

    /// Inserta una clave que viene de un PEER (ETSI-020): nunca pisa una
    /// entrada existente y respeta la cota por origen (B-01, B-02/R4). Un
    /// `key_id` ya presente —el nuestro o el de otro peer— es un conflicto,
    /// no una actualización: era la primitiva con la que un co-destinatario
    /// de un multicast sustituía la clave de sesión de un SAE local.
    pub fn insert_from_peer(
        &self,
        key_id: KeyId,
        origin: &str,
        initiator: SaeId,
        authorized: HashSet<SaeId>,
        material: Vec<u8>,
        ttl: Option<Duration>,
    ) -> std::result::Result<(), PendingInsertError> {
        let ttl = ttl.unwrap_or(self.default_ttl).min(MAX_TTL);
        let expires_at = Instant::now()
            .checked_add(ttl)
            .unwrap_or_else(|| Instant::now() + MAX_TTL);
        let mut count = self.per_origin.entry(origin.to_owned()).or_insert(0);
        if *count >= MAX_PENDING_PER_ORIGIN {
            return Err(PendingInsertError::OriginFull);
        }
        match self.inner.entry(key_id) {
            dashmap::mapref::entry::Entry::Occupied(_) => Err(PendingInsertError::Occupied),
            dashmap::mapref::entry::Entry::Vacant(v) => {
                v.insert(PendingEntry {
                    initiator,
                    authorized,
                    retrieved: HashSet::new(),
                    material: Zeroizing::new(material),
                    expires_at,
                    origin: Some(origin.to_owned()),
                });
                *count += 1;
                Ok(())
            }
        }
    }

    /// Inserta una clave que estará disponible para los SAEs de
    /// `authorized` durante `ttl` (si es `None` se usa el TTL por defecto).
    pub fn insert(
        &self,
        key_id: KeyId,
        initiator: SaeId,
        authorized: HashSet<SaeId>,
        material: Vec<u8>,
        ttl: Option<Duration>,
    ) {
        // Cota dura + suma comprobada (auditoría 2026-09b B1): el
        // `ttl_seconds` de `extension_mandatory` lo elige el peer por ETSI-020
        // y llegaba a `Instant::now() + Duration` sin comprobar; un valor
        // cercano a u64::MAX hacía overflow → panic → con `panic = "abort"`
        // caía el DKMS entero. 7 días cubre cualquier TTL legítimo.
        let ttl = ttl.unwrap_or(self.default_ttl).min(MAX_TTL);
        let expires_at = Instant::now()
            .checked_add(ttl)
            .unwrap_or_else(|| Instant::now() + MAX_TTL);
        let entry = PendingEntry {
            initiator,
            authorized,
            retrieved: HashSet::new(),
            material: Zeroizing::new(material),
            expires_at,
            origin: None,
        };
        if let Some(old) = self.inner.insert(key_id, entry) {
            self.note_removed(&old);
        }
    }

    /// Recupera *K* para `sae`. Devuelve `(material, initiator)` para que el
    /// handler ETSI pueda armar la respuesta. Tras servir al último SAE
    /// autorizado, la entrada se borra (zeroize automático).
    pub fn take_for_sae(&self, key_id: &KeyId, sae: &SaeId) -> Result<(Zeroizing<Vec<u8>>, SaeId)> {
        self.take_for_sae_of_master(key_id, sae, None)
    }

    /// Como [`Self::take_for_sae`], exigiendo además que el `master_SAE_ID`
    /// con el que el SAE pregunta sea el iniciador de la entrada (B-01): la
    /// semántica ETSI-014 es «clave compartida con ESE master», y sin esto un
    /// SAE no podía notar que la clave que recogía la puso otro. Mismo 404
    /// que una clave inexistente, para no revelar nada.
    pub fn take_for_sae_of_master(
        &self,
        key_id: &KeyId,
        sae: &SaeId,
        master: Option<&SaeId>,
    ) -> Result<(Zeroizing<Vec<u8>>, SaeId)> {
        let now = Instant::now();

        // Fase 1: bajo el lock, validar y marcar como entregada.
        let outcome = {
            let mut guard = self
                .inner
                .get_mut(key_id)
                .ok_or_else(|| DkmsError::KeyNotAuthorized { sae: sae.clone() })?;

            if guard.is_expired(now) {
                // dropear el guard antes de remove para no auto-bloquearnos.
                drop(guard);
                self.remove_entry(key_id);
                return Err(DkmsError::KeyExpired);
            }
            if !guard.authorized.contains(sae) {
                return Err(DkmsError::KeyNotAuthorized { sae: sae.clone() });
            }
            if let Some(m) = master {
                if guard.initiator != *m {
                    return Err(DkmsError::KeyNotAuthorized { sae: sae.clone() });
                }
            }
            if guard.retrieved.contains(sae) {
                // ya consumido por este SAE — tratar como no autorizado para
                // no revelar que la clave existió.
                return Err(DkmsError::KeyNotAuthorized { sae: sae.clone() });
            }
            guard.retrieved.insert(sae.clone());

            let copy: Zeroizing<Vec<u8>> = Zeroizing::new(guard.material.to_vec());
            let initiator = guard.initiator.clone();
            let complete = guard.is_complete();
            (copy, initiator, complete)
        };

        let (material, initiator, complete) = outcome;
        if complete {
            // Borrar de forma idempotente. Carrera benigna: si llegó otro
            // borrado por sweep, simplemente no hay entrada.
            self.remove_entry(key_id);
            trace!(%key_id, "pending entry exhausted, removed");
        }
        Ok((material, initiator))
    }

    /// Barrido manual de expiraciones. Devuelve cuántas entradas borró.
    pub fn sweep_expired(&self) -> usize {
        let now = Instant::now();
        let stale: Vec<KeyId> = self
            .inner
            .iter()
            .filter_map(|e| {
                if e.value().is_expired(now) {
                    Some(e.key().clone())
                } else {
                    None
                }
            })
            .collect();
        let n = stale.len();
        for k in stale {
            self.remove_entry(&k);
        }
        if n > 0 {
            debug!(removed = n, "pending sweep evicted expired entries");
        }
        n
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Borra todo (shutdown / drain).
    pub fn clear(&self) {
        self.inner.clear();
        self.per_origin.clear();
    }

    /// Borrado administrativo (bypassa autz/retrievals). Útil para limpiar
    /// claves que ya no van a entregarse — p. ej., una distribución que
    /// falló a un peer y queremos retractar las que metimos localmente.
    pub fn force_remove(&self, key_id: &KeyId) -> bool {
        self.remove_entry(key_id)
    }
}

/// Tarea de fondo que dispara `sweep_expired` periódicamente.
pub async fn sweeper_task(store: Arc<PendingStore>, period_secs: u64) {
    let mut tick = tokio::time::interval(Duration::from_secs(period_secs.max(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        store.sweep_expired();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saes(ids: &[&str]) -> HashSet<SaeId> {
        ids.iter().map(|s| SaeId::new(*s)).collect()
    }

    #[test]
    fn authorized_sae_gets_key_unauthorized_does_not() {
        let s = PendingStore::new(60);
        s.insert(
            KeyId::new("k1"),
            SaeId::new("master"),
            saes(&["alice", "bob"]),
            vec![1, 2, 3],
            None,
        );

        // alice está autorizada
        let (m, ini) = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("alice"))
            .unwrap();
        assert_eq!(&*m, &[1, 2, 3]);
        assert_eq!(ini.as_str(), "master");

        // eve no
        let err = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("eve"))
            .unwrap_err();
        assert!(matches!(err, DkmsError::KeyNotAuthorized { .. }));

        // bob sí (y al ser el último autorizado, la entrada se borra)
        let (m, _) = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("bob"))
            .unwrap();
        assert_eq!(&*m, &[1, 2, 3]);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn a_peer_cannot_overwrite_an_existing_pending_key() {
        // B-01: la entrada local (nuestro SAE) no la pisa un ext_keys ajeno
        // con el mismo key_id, y el material queda intacto.
        let s = PendingStore::new(60);
        s.insert(
            KeyId::new("K"),
            SaeId::new("m"),
            saes(&["alice"]),
            vec![1, 2, 3],
            None,
        );
        let err = s
            .insert_from_peer(
                KeyId::new("K"),
                "dkms-2",
                SaeId::new("evil"),
                saes(&["alice"]),
                vec![9, 9, 9],
                None,
            )
            .unwrap_err();
        assert_eq!(err, PendingInsertError::Occupied);
        let (m, ini) = s
            .take_for_sae(&KeyId::new("K"), &SaeId::new("alice"))
            .unwrap();
        assert_eq!(&*m, &[1, 2, 3]);
        assert_eq!(ini.as_str(), "m");
        assert_eq!(s.pending_from("dkms-2"), 0);
    }

    #[test]
    fn dec_keys_binds_the_master_to_the_initiator() {
        let s = PendingStore::new(60);
        s.insert(
            KeyId::new("K"),
            SaeId::new("m"),
            saes(&["alice"]),
            vec![1],
            None,
        );
        let err = s
            .take_for_sae_of_master(
                &KeyId::new("K"),
                &SaeId::new("alice"),
                Some(&SaeId::new("otro")),
            )
            .unwrap_err();
        assert!(matches!(err, DkmsError::KeyNotAuthorized { .. }));
        // El intento con master equivocado no consume la clave.
        let (m, _) = s
            .take_for_sae_of_master(
                &KeyId::new("K"),
                &SaeId::new("alice"),
                Some(&SaeId::new("m")),
            )
            .unwrap();
        assert_eq!(&*m, &[1]);
    }

    #[test]
    fn per_origin_cap_and_accounting() {
        let s = PendingStore::new(60);
        for i in 0..3 {
            s.insert_from_peer(
                KeyId::new(format!("k{i}")),
                "dkms-2",
                SaeId::new("m"),
                saes(&["a"]),
                vec![1],
                None,
            )
            .unwrap();
        }
        assert_eq!(s.pending_from("dkms-2"), 3);
        let _ = s.take_for_sae(&KeyId::new("k0"), &SaeId::new("a")).unwrap();
        assert_eq!(s.pending_from("dkms-2"), 2);
        assert!(s.force_remove(&KeyId::new("k1")));
        assert_eq!(s.pending_from("dkms-2"), 1);
        s.clear();
        assert_eq!(s.pending_from("dkms-2"), 0);
    }

    #[test]
    fn same_sae_cannot_take_twice() {
        let s = PendingStore::new(60);
        s.insert(
            KeyId::new("k1"),
            SaeId::new("m"),
            saes(&["alice", "bob"]),
            vec![1, 2, 3],
            None,
        );
        let _ = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("alice"))
            .unwrap();
        let err = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("alice"))
            .unwrap_err();
        assert!(matches!(err, DkmsError::KeyNotAuthorized { .. }));
        // bob todavía puede
        assert!(s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("bob"))
            .is_ok());
    }

    #[test]
    fn expired_keys_return_expired_error_and_get_evicted() {
        let s = PendingStore::new(0);
        s.insert(
            KeyId::new("k1"),
            SaeId::new("m"),
            saes(&["alice"]),
            vec![9, 9],
            Some(Duration::from_millis(1)),
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        let err = s
            .take_for_sae(&KeyId::new("k1"), &SaeId::new("alice"))
            .unwrap_err();
        assert!(matches!(err, DkmsError::KeyExpired));
        // se eliminó al detectar la expiración
        assert_eq!(s.len(), 0);
    }

    /// Un `ttl_seconds` absurdo (cercano a u64::MAX) elegido por el peer NO
    /// debe hacer panic al calcular `expires_at` (auditoría 2026-09b B1): con
    /// `panic = "abort"` un solo POST tumbaría el DKMS entero. La entrada se
    /// guarda (recortada a MAX_TTL) y no está vencida.
    #[test]
    fn insert_with_absurd_ttl_does_not_panic_and_is_capped() {
        let s = PendingStore::new(60);
        s.insert(
            KeyId::new("k-huge"),
            SaeId::new("m"),
            saes(&["alice"]),
            vec![0u8; 32],
            Some(Duration::from_secs(u64::MAX)),
        );
        assert_eq!(s.len(), 1);
        // No está vencida: se puede recuperar.
        assert!(s
            .take_for_sae(&KeyId::new("k-huge"), &SaeId::new("alice"))
            .is_ok());
    }
}
