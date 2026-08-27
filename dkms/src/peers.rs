//! Registro de peers DKMS, mutable en caliente.
//!
//! Hasta ahora los peers salían de `DkmsConfig.peers` y se leían desde dos
//! sitios que no se hablaban: el mapa `peer → orr_id` que el [`Generator`]
//! construye una vez, y el `PeerCfg` que [`crate::service::DkmsService`]
//! resuelve en cada envío ETSI-020. Los dos eran fijos, así que un DKMS nuevo
//! en la red no existía para los que ya estaban.
//!
//! Ahora ambos leen de aquí, y el anunciador actualiza este registro con lo que
//! le responde la SDN. Lo del `node.yml` sigue valiendo como **semilla**: se usa
//! al arrancar y mientras la SDN no conteste, y en cuanto lo hace manda ella.
//!
//! [`Generator`]: crate::control::Generator

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use tracing::info;

use crate::config::{PeerCfg, PeerTransport};

/// Lo que la SDN dice de un peer. Es menos de lo que cabe en un [`PeerCfg`]:
/// la SDN sabe dónde está y por qué ORR se le llega, pero no las políticas
/// locales (`max_hops`, `security_level`, `sni`), que se conservan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerFromSdn {
    pub dkms_id: String,
    pub orr_id: String,
    /// `ip:port` del listener ETSI-020.
    pub endpoint: String,
}

pub struct PeerRegistry {
    inner: ArcSwap<HashMap<String, PeerCfg>>,
    /// Peers del `node.yml`. **Nunca se retiran.** Mientras la SDN no los
    /// conozca todavía, su lista llega vacía o incompleta; quitarlos entonces
    /// desconectaría peers vivos por un simple retraso.
    local: std::collections::HashSet<String>,
    /// `peer → orr_id` cacheado. El generador lo consulta en su bucle, así que
    /// no interesa reconstruirlo en cada vuelta.
    orr: ArcSwap<HashMap<String, String>>,
}

fn derive_orr(peers: &HashMap<String, PeerCfg>) -> HashMap<String, String> {
    peers
        .iter()
        .filter(|(_, pc)| pc.transport == PeerTransport::Orr)
        .filter_map(|(id, pc)| pc.orr_id.clone().map(|o| (id.clone(), o)))
        .collect()
}

impl PeerRegistry {
    /// Arranca con lo que haya en el `node.yml`.
    pub fn from_config(seed: HashMap<String, PeerCfg>) -> Self {
        let orr = derive_orr(&seed);
        let local = seed.keys().cloned().collect();
        Self {
            inner: ArcSwap::from_pointee(seed),
            local,
            orr: ArcSwap::from_pointee(orr),
        }
    }

    pub fn get(&self, dkms_id: &str) -> Option<PeerCfg> {
        self.inner.load().get(dkms_id).cloned()
    }

    pub fn snapshot(&self) -> Arc<HashMap<String, PeerCfg>> {
        self.inner.load_full()
    }

    /// `peer → orr_id` de los peers alcanzables por transporte ORR, que es lo
    /// que el generador necesita para repartir el relleno de buffers.
    pub fn orr_map(&self) -> Arc<HashMap<String, String>> {
        self.orr.load_full()
    }

    /// Aplica la lista que manda la SDN. Devuelve `true` si algo cambió.
    ///
    /// Las políticas locales de un peer que ya conocíamos se conservan: la SDN
    /// dice **dónde** está y por qué ORR, no con qué `max_hops` ni con qué
    /// nivel de seguridad hablarle. Un peer que desaparece de la lista se
    /// retira.
    pub fn apply_from_sdn(&self, peers: &[PeerFromSdn]) -> bool {
        let current = self.inner.load();
        // Se arranca de los locales, que sobreviven pase lo que pase, y encima
        // se aplica lo de la SDN.
        let mut next: HashMap<String, PeerCfg> = current
            .iter()
            .filter(|(id, _)| self.local.contains(*id))
            .map(|(id, pc)| (id.clone(), pc.clone()))
            .collect();
        for p in peers {
            let endpoint = if p.endpoint.contains("//") {
                p.endpoint.clone()
            } else {
                format!("https://{}", p.endpoint)
            };
            match current.get(&p.dkms_id) {
                Some(old) => next.insert(
                    p.dkms_id.clone(),
                    PeerCfg {
                        endpoint,
                        orr_id: Some(p.orr_id.clone()),
                        ..old.clone()
                    },
                ),
                None => next.insert(
                    p.dkms_id.clone(),
                    PeerCfg {
                        endpoint,
                        orr_id: Some(p.orr_id.clone()),
                        transport: PeerTransport::Orr,
                        sni: None,
                        max_hops: None,
                        orr_path: None,
                        security_level: None,
                    },
                ),
            };
        }
        if **current == next {
            return false;
        }
        let added: Vec<_> = next.keys().filter(|k| !current.contains_key(*k)).collect();
        let removed: Vec<_> = current.keys().filter(|k| !next.contains_key(*k)).collect();
        info!(
            ?added,
            ?removed,
            n = next.len(),
            "peers actualizados por la SDN"
        );
        self.orr.store(Arc::new(derive_orr(&next)));
        self.inner.store(Arc::new(next));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_peer(endpoint: &str, max_hops: Option<i32>) -> PeerCfg {
        PeerCfg {
            endpoint: endpoint.into(),
            sni: None,
            orr_id: Some("orr_9".into()),
            transport: PeerTransport::Orr,
            max_hops,
            orr_path: None,
            security_level: None,
        }
    }

    fn from_sdn(id: &str, orr: &str, ep: &str) -> PeerFromSdn {
        PeerFromSdn {
            dkms_id: id.into(),
            orr_id: orr.into(),
            endpoint: ep.into(),
        }
    }

    #[test]
    fn applying_the_same_list_twice_reports_no_change() {
        let r = PeerRegistry::from_config(HashMap::new());
        let list = [from_sdn("dkms-2", "orr_2", "10.0.0.2:20006")];
        assert!(r.apply_from_sdn(&list));
        // El anunciador llama a esto en cada latido. Si un aplique idéntico
        // dijera que cambió, habría altas y bajas en bucle.
        for _ in 0..5 {
            assert!(!r.apply_from_sdn(&list));
        }
    }

    #[test]
    fn local_policy_survives_an_sdn_update() {
        let mut seed = HashMap::new();
        seed.insert(
            "dkms-2".to_string(),
            seed_peer("https://viejo:20006", Some(3)),
        );
        let r = PeerRegistry::from_config(seed);

        r.apply_from_sdn(&[from_sdn("dkms-2", "orr_2", "10.0.0.2:20006")]);
        let p = r.get("dkms-2").unwrap();
        // La SDN manda en dónde está y por qué ORR...
        assert_eq!(p.endpoint, "https://10.0.0.2:20006");
        assert_eq!(p.orr_id.as_deref(), Some("orr_2"));
        // ...pero no en la política local.
        assert_eq!(p.max_hops, Some(3));
    }

    #[test]
    fn a_seeded_peer_survives_an_empty_sdn_list() {
        let mut seed = HashMap::new();
        seed.insert(
            "dkms-2".to_string(),
            seed_peer("https://10.0.0.2:20006", None),
        );
        let r = PeerRegistry::from_config(seed);

        // Mientras la SDN no conozca todavía a dkms-2, su lista llega vacía.
        // Detectado en el laboratorio: el QKC llegó a tirar enlaces vivos por
        // esto, destruyendo su material de clave, y se recuperaba 30 s después.
        r.apply_from_sdn(&[]);
        assert!(
            r.get("dkms-2").is_some(),
            "lo del node.yml es un suelo, no algo que la SDN pueda borrar por ir retrasada"
        );
    }

    #[test]
    fn a_peer_that_leaves_the_list_is_dropped() {
        let r = PeerRegistry::from_config(HashMap::new());
        r.apply_from_sdn(&[
            from_sdn("dkms-2", "orr_2", "10.0.0.2:20006"),
            from_sdn("dkms-3", "orr_3", "10.0.0.3:20006"),
        ]);
        assert!(r.apply_from_sdn(&[from_sdn("dkms-2", "orr_2", "10.0.0.2:20006")]));
        assert!(r.get("dkms-3").is_none());
        assert_eq!(r.orr_map().len(), 1);
    }
}

/// Con qué **ejecución** de cada peer estamos hablando.
///
/// Los buffers del DKMS son sólo RAM, así que un peer que se reinicia vuelve
/// sin nada, y nuestra copia del material que compartíamos con él queda
/// inservible: `buffer_enc[peer]` aquí es el mismo material que
/// `buffer_dec[yo]` allí. Cada `DKMS_BUFFER` lleva la encarnación del
/// emisor ([`crate::southbound::orr::HDR_INCARNATION`]), y cuando cambia es
/// que se reinició.
///
/// Se mira sobre el tráfico de relleno del propio peer y no sobre los ACK: un
/// DKMS que arranca tiene el ENC vacío, así que emite enseguida, mientras que
/// los ACK sólo llegan si nosotros emitimos — y si estamos parados en el tope
/// no emitimos, que es exactamente la situación de la que hay que salir.
#[derive(Debug, Default)]
pub struct PeerIncarnations {
    seen: DashMap<String, String>,
    /// Último wipe por peer, para rate-limitar (ver `note_at`).
    last_wipe: DashMap<String, std::time::Instant>,
}

impl PeerIncarnations {
    pub fn new() -> Self {
        Self::default()
    }

    /// Anota la encarnación que acabamos de oír de `peer`.
    ///
    /// Devuelve `Some(anterior)` **sólo** si el peer se ha reiniciado. La
    /// primera vez que se le oye no lo es: es que acabamos de arrancar
    /// nosotros, y tirar el buffer ahí sería tirar material bueno en cada
    /// arranque. Sin cooldown (equivale a `note_at` con cooldown 0).
    pub fn note(&self, peer: &str, incarnation: &str) -> Option<String> {
        self.note_at(peer, incarnation, std::time::Instant::now(), Duration::ZERO)
    }

    /// Igual que [`Self::note`] pero rate-limitando los wipes: si ya hicimos
    /// uno para este peer hace menos de `cooldown`, no devuelve `Some` aunque
    /// la encarnación haya cambiado. `incarnation` es un id aleatorio por
    /// proceso y llega en un header de la entrega ORR **sin autenticar** (el
    /// salto ORR es intra-institución, §1.2): un peer que la cambie en cada
    /// mensaje podría, si no, vaciar nuestro buffer en bucle (DoS). Un
    /// reinicio legítimo la cambia UNA vez, así que el cooldown no lo estorba.
    pub fn note_at(
        &self,
        peer: &str,
        incarnation: &str,
        now: std::time::Instant,
        cooldown: Duration,
    ) -> Option<String> {
        let previous = self
            .seen
            .insert(peer.to_string(), incarnation.to_string())?;
        if previous == incarnation {
            return None;
        }
        if let Some(last) = self.last_wipe.get(peer) {
            if now.duration_since(*last) < cooldown {
                return None; // rate-limited: cambio demasiado frecuente
            }
        }
        self.last_wipe.insert(peer.to_string(), now);
        Some(previous)
    }
}

#[cfg(test)]
mod incarnation_tests {
    use std::time::{Duration, Instant};

    use super::PeerIncarnations;

    #[test]
    fn wipe_is_rate_limited_within_cooldown() {
        // Un peer que cambia la incarnation en cada mensaje no puede tirar el
        // buffer en bucle: solo el primer cambio (y luego uno por cooldown).
        let inc = PeerIncarnations::new();
        let t0 = Instant::now();
        let cd = Duration::from_secs(30);
        inc.note_at("dkms-2", "a", t0, cd); // primera vista, no wipe
        // Ráfaga de cambios dentro del cooldown: solo el primero dispara wipe.
        assert_eq!(
            inc.note_at("dkms-2", "b", t0 + Duration::from_secs(1), cd),
            Some("a".to_string()),
            "el primer cambio sí tira el material viejo",
        );
        assert_eq!(
            inc.note_at("dkms-2", "c", t0 + Duration::from_secs(2), cd),
            None,
            "un segundo cambio dentro del cooldown se ignora",
        );
        assert_eq!(
            inc.note_at("dkms-2", "d", t0 + Duration::from_secs(3), cd),
            None,
        );
        // Pasado el cooldown desde el último wipe, un cambio real vuelve a tirar.
        assert_eq!(
            inc.note_at("dkms-2", "e", t0 + Duration::from_secs(40), cd),
            Some("d".to_string()),
            "tras el cooldown un reinicio legítimo se atiende",
        );
    }

    #[test]
    fn the_first_sighting_of_a_peer_is_not_a_restart() {
        let inc = PeerIncarnations::new();
        assert_eq!(inc.note("dkms-2", "aaaa"), None, "acabamos de arrancar");
    }

    #[test]
    fn the_same_incarnation_is_not_a_restart() {
        let inc = PeerIncarnations::new();
        inc.note("dkms-2", "aaaa");
        for _ in 0..5 {
            assert_eq!(
                inc.note("dkms-2", "aaaa"),
                None,
                "cada clave que llega no puede tirar el buffer",
            );
        }
    }

    #[test]
    fn a_new_incarnation_reports_the_one_it_replaces() {
        let inc = PeerIncarnations::new();
        inc.note("dkms-2", "aaaa");
        assert_eq!(inc.note("dkms-2", "bbbb"), Some("aaaa".to_string()));
        // Y una sola vez: el resto del relleno no vuelve a tirar nada.
        assert_eq!(inc.note("dkms-2", "bbbb"), None);
    }

    #[test]
    fn peers_are_tracked_independently() {
        let inc = PeerIncarnations::new();
        inc.note("dkms-2", "aaaa");
        inc.note("dkms-3", "cccc");
        assert_eq!(inc.note("dkms-2", "bbbb"), Some("aaaa".to_string()));
        assert_eq!(inc.note("dkms-3", "cccc"), None, "el otro no se ha movido");
    }

    /// Un peer que la SDN retira y devuelve NO se olvida: sus buffers siguen
    /// aquí, así que su vuelta con otra encarnación tiene que seguir tirando
    /// el material viejo. Olvidarle sería justo saltarse esa limpieza.
    #[test]
    fn a_peer_that_leaves_and_returns_is_still_a_restart() {
        let inc = PeerIncarnations::new();
        inc.note("dkms-2", "aaaa");
        assert_eq!(inc.note("dkms-2", "bbbb"), Some("aaaa".to_string()));
    }
}
