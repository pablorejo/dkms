//! Forwarding table en `ArcSwap`.
//!
//! Lecturas: un atomic-load (lock-free).
//! Escrituras: construyo HashMap nuevo + swap atómico del `Arc`.
//!
//! Convenciones:
//!
//! * Si `dest_id` es **vecino directo** (presente en `direct_neighbors`),
//!   el next-hop es el propio destino sin consultar la tabla.
//! * Si no, se mira el `HashMap<u32, u32>` de la tabla.
//! * Si no hay entry → `None` → caller responde error de routing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;

pub struct ForwardingTable {
    /// dest_id → next_hop_id (ambos QKC u32).
    table: ArcSwap<HashMap<u32, u32>>,
    /// Vecinos directos: si dest_id está aquí, no necesitamos tabla.
    direct: HashSet<u32>,
}

impl ForwardingTable {
    pub fn new(direct_neighbors: HashSet<u32>) -> Self {
        Self {
            table: ArcSwap::from_pointee(HashMap::new()),
            direct: direct_neighbors,
        }
    }

    /// Decide el siguiente salto. `None` si no hay ruta (caller suelta el frame).
    #[inline]
    pub fn next_hop(&self, dest_id: u32) -> Option<u32> {
        if self.direct.contains(&dest_id) {
            return Some(dest_id);
        }
        self.table.load().get(&dest_id).copied()
    }

    /// Reemplaza la tabla entera. Atómico — los lectores que estén
    /// resolviendo en este instante terminan con la versión vieja.
    pub fn replace(&self, new: HashMap<u32, u32>) {
        self.table.store(Arc::new(new));
    }

    /// Aplica un delta (clone-modify-swap).
    pub fn update(&self, updates: HashMap<u32, u32>, removes: &[u32]) {
        let mut next = (**self.table.load()).clone();
        for (dest, hop) in updates {
            next.insert(dest, hop);
        }
        for d in removes {
            next.remove(d);
        }
        self.table.store(Arc::new(next));
    }

    /// Snapshot (clones the map — no usar en hot path).
    pub fn snapshot(&self) -> HashMap<u32, u32> {
        (**self.table.load()).clone()
    }

    pub fn is_direct_neighbor(&self, peer_id: u32) -> bool {
        self.direct.contains(&peer_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_neighbor_short_circuits() {
        let ft = ForwardingTable::new([2u32, 3].into_iter().collect());
        assert_eq!(ft.next_hop(2), Some(2));
        assert_eq!(ft.next_hop(3), Some(3));
    }

    #[test]
    fn table_lookup_returns_next_hop() {
        let ft = ForwardingTable::new([2u32].into_iter().collect());
        let mut updates = HashMap::new();
        updates.insert(5, 2);
        updates.insert(7, 2);
        ft.update(updates, &[]);
        assert_eq!(ft.next_hop(5), Some(2));
        assert_eq!(ft.next_hop(7), Some(2));
        assert_eq!(ft.next_hop(9), None);
    }

    #[test]
    fn replace_atomic() {
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t1 = HashMap::new();
        t1.insert(5, 2);
        ft.replace(t1);
        assert_eq!(ft.next_hop(5), Some(2));
        let t2 = HashMap::new();
        ft.replace(t2);
        assert_eq!(ft.next_hop(5), None);
    }
}
