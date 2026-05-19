//! Weighted Cost Multi-Path (WCMP) forwarding table.
//!
//! Holds a `dest_qkc → Vec<NextHop>` map in an [`ArcSwap`] so reads
//! are lock-free (one atomic load) and writes are clone-modify-swap.
//! When the destination is a direct neighbour the table is skipped
//! and the destination itself is the next hop.
//!
//! ## Selection
//!
//! For a destination with multiple weighted next hops, the choice is
//! deterministic in the caller-provided `hash` value — typically a
//! hash of the frame's first key id. This gives **flow affinity**:
//! every frame carrying a given key takes the same path, so reorder
//! semantics inside a "logical key flow" are preserved without any
//! coordination between QKCs. Weights are interpreted as relative
//! shares: a list `[(A, 3), (B, 1)]` sends 75 % of (hash space) to
//! A and 25 % to B.
//!
//! ## Compatibility
//!
//! A single-path table (the only thing the legacy SDN pushed before
//! phase 4) is just a WCMP table where every destination has one
//! entry with `weight = 1`. The HTTP admin layer accepts both wire
//! shapes (`u32` or `Vec<NextHop>`) so the demo scripts that POST
//! `{"replace": {"22": 0}}` keep working without changes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

/// One weighted next-hop choice in a WCMP entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NextHop {
    pub qkc_id: u32,
    /// Relative weight (≥ 1). Zero-weight entries are stripped at
    /// `replace`/`update` time so the runtime selection logic can
    /// assume all weights are positive.
    pub weight: u32,
}

impl NextHop {
    pub fn single(qkc_id: u32) -> Self {
        Self { qkc_id, weight: 1 }
    }
}

pub struct ForwardingTable {
    /// `dest_qkc → ordered list of weighted next hops`. Empty `Vec`
    /// means "destination known but no path" (would surface as
    /// `next_hop = None` to the caller).
    table: ArcSwap<HashMap<u32, Vec<NextHop>>>,
    /// Direct neighbours short-circuit table lookup.
    direct: HashSet<u32>,
}

impl ForwardingTable {
    pub fn new(direct_neighbors: HashSet<u32>) -> Self {
        Self {
            table: ArcSwap::from_pointee(HashMap::new()),
            direct: direct_neighbors,
        }
    }

    /// Resolve the next hop for `dest_id`. `hash` is consumed
    /// modulo `sum_weights` to pick which next-hop in the WCMP
    /// entry receives this frame — passing the same `hash` always
    /// returns the same hop. For single-next-hop entries `hash`
    /// is ignored.
    #[inline]
    pub fn next_hop(&self, dest_id: u32, hash: u64) -> Option<u32> {
        if self.direct.contains(&dest_id) {
            return Some(dest_id);
        }
        let table = self.table.load();
        let entries = table.get(&dest_id)?;
        pick(entries, hash)
    }

    /// Replace the whole table. Entries with `weight = 0` are
    /// filtered out — they would never be selected and complicate
    /// the runtime branch.
    pub fn replace(&self, new: HashMap<u32, Vec<NextHop>>) {
        let sanitised = sanitise(new);
        self.table.store(Arc::new(sanitised));
    }

    /// Apply a delta: insert/replace per-destination entries and
    /// drop the destinations in `removes`. Atomic via
    /// clone-modify-swap.
    pub fn update(&self, updates: HashMap<u32, Vec<NextHop>>, removes: &[u32]) {
        let mut next = (**self.table.load()).clone();
        for (dest, hops) in sanitise(updates) {
            next.insert(dest, hops);
        }
        for d in removes {
            next.remove(d);
        }
        self.table.store(Arc::new(next));
    }

    /// Whole-table snapshot. Clones the inner map — don't use in
    /// hot paths.
    pub fn snapshot(&self) -> HashMap<u32, Vec<NextHop>> {
        (**self.table.load()).clone()
    }

    pub fn is_direct_neighbor(&self, peer_id: u32) -> bool {
        self.direct.contains(&peer_id)
    }
}

/// Drop empty entries and zero-weight hops. Keeps the runtime
/// selection branch-free.
fn sanitise(t: HashMap<u32, Vec<NextHop>>) -> HashMap<u32, Vec<NextHop>> {
    t.into_iter()
        .map(|(dst, hops)| (dst, hops.into_iter().filter(|h| h.weight > 0).collect()))
        .filter(|(_, hops): &(_, Vec<NextHop>)| !hops.is_empty())
        .collect()
}

/// Pick one entry by hash. Empty `entries` → `None`; single entry
/// short-circuits; otherwise hash-bucket selection across the
/// cumulative weight axis.
fn pick(entries: &[NextHop], hash: u64) -> Option<u32> {
    match entries.len() {
        0 => None,
        1 => Some(entries[0].qkc_id),
        _ => {
            let total: u64 = entries.iter().map(|h| h.weight as u64).sum();
            if total == 0 {
                // Defensive: sanitise should have removed these.
                return Some(entries[0].qkc_id);
            }
            let bucket = hash % total;
            let mut acc: u64 = 0;
            for h in entries {
                acc += h.weight as u64;
                if bucket < acc {
                    return Some(h.qkc_id);
                }
            }
            // Floating-point-free arithmetic → unreachable in practice,
            // but defensive last-entry pick keeps the function total.
            Some(entries.last().unwrap().qkc_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(pairs: &[(u32, u32)]) -> Vec<NextHop> {
        pairs
            .iter()
            .map(|(qkc_id, weight)| NextHop {
                qkc_id: *qkc_id,
                weight: *weight,
            })
            .collect()
    }

    #[test]
    fn direct_neighbor_short_circuits_table() {
        let ft = ForwardingTable::new([2u32, 3].into_iter().collect());
        assert_eq!(ft.next_hop(2, 0), Some(2));
        assert_eq!(ft.next_hop(3, 12345), Some(3));
    }

    #[test]
    fn single_next_hop_works() {
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t = HashMap::new();
        t.insert(9, entries(&[(2, 1)]));
        ft.replace(t);
        // Regardless of hash, single entry always wins.
        assert_eq!(ft.next_hop(9, 0), Some(2));
        assert_eq!(ft.next_hop(9, u64::MAX), Some(2));
    }

    #[test]
    fn weighted_selection_obeys_relative_proportions() {
        // Two next-hops with weights 3:1. Hash-bucket selection over
        // many samples should hit A roughly 75 % of the time.
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t = HashMap::new();
        t.insert(9, entries(&[(10, 3), (20, 1)]));
        ft.replace(t);

        let n = 10_000u64;
        let mut count_10 = 0u64;
        for h in 0..n {
            if ft.next_hop(9, h) == Some(10) {
                count_10 += 1;
            }
        }
        let ratio = count_10 as f64 / n as f64;
        assert!((ratio - 0.75).abs() < 0.01, "expected ~0.75, got {ratio}");
    }

    #[test]
    fn same_hash_always_same_next_hop_flow_affinity() {
        // Flow affinity: a given hash always resolves to the same
        // hop. Cards on the table for QKD where reorder inside a
        // "logical key flow" should be avoided.
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t = HashMap::new();
        t.insert(9, entries(&[(10, 1), (20, 1), (30, 1)]));
        ft.replace(t);
        for h in [7u64, 42, 1_000, u64::MAX / 2] {
            let first = ft.next_hop(9, h);
            for _ in 0..5 {
                assert_eq!(ft.next_hop(9, h), first);
            }
        }
    }

    #[test]
    fn unknown_destination_returns_none() {
        let ft = ForwardingTable::new([].into_iter().collect());
        assert_eq!(ft.next_hop(99, 0), None);
    }

    #[test]
    fn replace_drops_zero_weight_entries() {
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t = HashMap::new();
        t.insert(9, entries(&[(10, 0), (20, 5)]));
        ft.replace(t);
        let snap = ft.snapshot();
        // 10 stripped, 20 kept.
        assert_eq!(snap.get(&9).unwrap().len(), 1);
        assert_eq!(snap.get(&9).unwrap()[0].qkc_id, 20);
    }

    #[test]
    fn replace_drops_entries_with_no_hops() {
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t = HashMap::new();
        t.insert(9, entries(&[(10, 0), (20, 0)])); // all dropped → entry empty
        t.insert(99, entries(&[(100, 1)]));
        ft.replace(t);
        let snap = ft.snapshot();
        // dest 9 disappears entirely; 99 stays.
        assert!(!snap.contains_key(&9));
        assert!(snap.contains_key(&99));
    }

    #[test]
    fn update_applies_delta_and_removes() {
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut initial = HashMap::new();
        initial.insert(5, entries(&[(1, 1)]));
        initial.insert(7, entries(&[(2, 1)]));
        ft.replace(initial);

        let mut upd = HashMap::new();
        upd.insert(5, entries(&[(3, 2), (4, 1)])); // re-route 5
        upd.insert(8, entries(&[(2, 1)])); // new entry
        ft.update(upd, &[7]); // drop 7

        assert_eq!(ft.next_hop(7, 0), None);
        let s = ft.snapshot();
        assert_eq!(s.get(&5).unwrap().len(), 2);
        assert_eq!(s.get(&8).unwrap()[0].qkc_id, 2);
    }

    #[test]
    fn replace_is_atomic_against_observers() {
        // Behavioural test: a sequence of replaces never leaves a
        // reader with a half-applied table. Doesn't catch races
        // by itself; mostly here to document the contract.
        let ft = ForwardingTable::new([].into_iter().collect());
        let mut t1 = HashMap::new();
        t1.insert(5, entries(&[(2, 1)]));
        ft.replace(t1);
        assert_eq!(ft.next_hop(5, 0), Some(2));
        let t2 = HashMap::new();
        ft.replace(t2);
        assert_eq!(ft.next_hop(5, 0), None);
    }
}
