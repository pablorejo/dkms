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
}

impl PendingStore {
    pub fn new(default_ttl_secs: u64) -> Self {
        Self {
            inner: DashMap::new(),
            default_ttl: Duration::from_secs(default_ttl_secs),
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
        };
        self.inner.insert(key_id, entry);
    }

    /// Recupera *K* para `sae`. Devuelve `(material, initiator)` para que el
    /// handler ETSI pueda armar la respuesta. Tras servir al último SAE
    /// autorizado, la entrada se borra (zeroize automático).
    pub fn take_for_sae(&self, key_id: &KeyId, sae: &SaeId) -> Result<(Zeroizing<Vec<u8>>, SaeId)> {
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
                self.inner.remove(key_id);
                return Err(DkmsError::KeyExpired);
            }
            if !guard.authorized.contains(sae) {
                return Err(DkmsError::KeyNotAuthorized { sae: sae.clone() });
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
            self.inner.remove(key_id);
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
            self.inner.remove(&k);
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
    }

    /// Borrado administrativo (bypassa autz/retrievals). Útil para limpiar
    /// claves que ya no van a entregarse — p. ej., una distribución que
    /// falló a un peer y queremos retractar las que metimos localmente.
    pub fn force_remove(&self, key_id: &KeyId) -> bool {
        self.inner.remove(key_id).is_some()
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
