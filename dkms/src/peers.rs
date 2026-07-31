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

use arc_swap::ArcSwap;
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
