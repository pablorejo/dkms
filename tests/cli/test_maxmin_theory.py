"""Unit tests for ``tests/cli/maxmin_theory``."""

from __future__ import annotations

import math

import pytest

from tests.cli.maxmin_theory import (
    DEFAULT_EFFECTIVE_ALPHA,
    all_pairs_commodities,
    path_edges,
    shortest_path,
    topology_to_capacities,
    topology_to_graph,
    weighted_maxmin,
)
from tests.cli.topology_builders import (
    DEFAULT_R0,
    build_mesh,
    build_ring,
    build_star,
)


# -----------------------------------------------------------------------------
# Graph / path helpers
# -----------------------------------------------------------------------------


def test_shortest_path_src_equals_dst() -> None:
    g = {"a": ["b"], "b": ["a"]}
    assert shortest_path(g, "a", "a") == ["a"]


def test_shortest_path_direct_neighbour() -> None:
    g = {"a": ["b"], "b": ["a"]}
    assert shortest_path(g, "a", "b") == ["a", "b"]


def test_shortest_path_two_hops() -> None:
    g = {"a": ["b"], "b": ["a", "c"], "c": ["b"]}
    assert shortest_path(g, "a", "c") == ["a", "b", "c"]


def test_shortest_path_disconnected() -> None:
    g = {"a": [], "b": []}
    assert shortest_path(g, "a", "b") is None


def test_shortest_path_unknown_endpoint() -> None:
    g = {"a": ["b"], "b": ["a"]}
    assert shortest_path(g, "a", "zzz") is None


def test_path_edges_canonical() -> None:
    assert path_edges(["node-1", "node-2", "node-3"]) == [
        ("node-1", "node-2"),
        ("node-2", "node-3"),
    ]
    # Out-of-order uids should still produce canonical (min, max) tuples
    assert path_edges(["b", "a", "c"]) == [("a", "b"), ("a", "c")]


def test_path_edges_empty_or_single() -> None:
    assert path_edges([]) == []
    assert path_edges(["only"]) == []


# -----------------------------------------------------------------------------
# Topology helpers
# -----------------------------------------------------------------------------


def test_topology_to_graph_ring4() -> None:
    topo = build_ring(4)
    g = topology_to_graph(topo)
    assert set(g.keys()) == {"node-1", "node-2", "node-3", "node-4"}
    # Each node should have exactly 2 neighbours in a ring
    for uid, nbrs in g.items():
        assert len(nbrs) == 2, f"{uid} should have 2 neighbours, got {nbrs}"


def test_topology_to_capacities_formula() -> None:
    topo = build_ring(3)
    caps = topology_to_capacities(topo, alpha=0.2)
    # Builder default: distance_km=5, R0=2000
    expected = DEFAULT_R0 * math.pow(10.0, -0.2 * 5 / 10.0)
    for cap in caps.values():
        assert abs(cap - expected) < 1e-6


def test_topology_to_capacities_alpha_zero() -> None:
    topo = build_ring(3)
    caps = topology_to_capacities(topo, alpha=0.0)
    for cap in caps.values():
        assert abs(cap - DEFAULT_R0) < 1e-6


def test_topology_to_capacities_default_alpha_is_02() -> None:
    assert DEFAULT_EFFECTIVE_ALPHA == 0.2


# -----------------------------------------------------------------------------
# Closed-form max-min cases
# -----------------------------------------------------------------------------


def test_single_edge_two_flows_equal_share() -> None:
    caps = {("a", "b"): 100.0}
    comms = [
        {"id": "a->b", "path": ["a", "b"]},
        {"id": "b->a", "path": ["b", "a"]},
    ]
    r = weighted_maxmin(comms, caps)
    assert abs(r["a->b"] - 50.0) < 1e-6
    assert abs(r["b->a"] - 50.0) < 1e-6


def test_single_edge_weighted_share() -> None:
    caps = {("a", "b"): 100.0}
    comms = [
        {"id": "a->b", "path": ["a", "b"]},
        {"id": "b->a", "path": ["b", "a"]},
    ]
    r = weighted_maxmin(comms, caps, weights={"a->b": 2.0, "b->a": 1.0})
    assert abs(r["a->b"] - 200.0 / 3.0) < 1e-6
    assert abs(r["b->a"] - 100.0 / 3.0) < 1e-6


def test_zero_weight_is_inactive() -> None:
    caps = {("a", "b"): 100.0}
    comms = [
        {"id": "a->b", "path": ["a", "b"]},
        {"id": "b->a", "path": ["b", "a"]},
    ]
    r = weighted_maxmin(comms, caps, weights={"a->b": 0.0, "b->a": 1.0})
    assert "a->b" not in r
    assert abs(r["b->a"] - 100.0) < 1e-6


def test_commodity_without_path_is_inactive() -> None:
    caps = {("a", "b"): 100.0}
    comms = [
        {"id": "a->b", "path": ["a", "b"]},
        {"id": "orphan", "path": []},
    ]
    r = weighted_maxmin(comms, caps)
    assert "orphan" not in r
    assert abs(r["a->b"] - 100.0) < 1e-6


def test_three_flows_no_shared_edges() -> None:
    # Three independent edges, three commodities each on its own edge.
    caps = {
        ("a", "b"): 100.0,
        ("c", "d"): 200.0,
        ("e", "f"): 300.0,
    }
    comms = [
        {"id": "ab", "path": ["a", "b"]},
        {"id": "cd", "path": ["c", "d"]},
        {"id": "ef", "path": ["e", "f"]},
    ]
    r = weighted_maxmin(comms, caps)
    assert abs(r["ab"] - 100.0) < 1e-6
    assert abs(r["cd"] - 200.0) < 1e-6
    assert abs(r["ef"] - 300.0) < 1e-6


def test_empty_inputs_return_empty() -> None:
    assert weighted_maxmin([], {("a", "b"): 1.0}) == {}
    assert weighted_maxmin([{"id": "x", "path": ["a", "b"]}], {}) == {}


# -----------------------------------------------------------------------------
# Topology-driven scenarios (ring 4, mesh 2x2, star 2x2)
# -----------------------------------------------------------------------------


def _capacity_respected(comms, caps, rates) -> bool:
    """Sum of rates per edge across all commodities must not exceed cap."""
    load = {ek: 0.0 for ek in caps}
    for c in comms:
        r = rates.get(c["id"], 0.0)
        if r <= 0:
            continue
        for ek in path_edges(c["path"]):
            if ek in load:
                load[ek] += r
    for ek, cap in caps.items():
        if load[ek] > cap + 1e-6:
            return False
    return True


def test_ring_4_rates_positive_and_capacity_respected() -> None:
    topo = build_ring(4)
    g = topology_to_graph(topo)
    caps = topology_to_capacities(topo)
    comms = all_pairs_commodities(g)
    # 4 nodes * 3 destinations = 12 ordered pairs
    assert len(comms) == 12
    rates = weighted_maxmin(comms, caps)
    # All commodities should get a positive rate
    assert len(rates) == 12
    for r in rates.values():
        assert r > 0.0
    assert _capacity_respected(comms, caps, rates)


def test_ring_4_symmetric_directional_pairs() -> None:
    """In a ring with uniform weights, ``A→B`` and ``B→A`` must get the same rate.

    Both directions use the same BFS path (BFS is deterministic given a fixed
    adjacency order), so they share the same set of bottleneck edges and the
    progressive-filling algorithm grows them in lockstep.
    """
    topo = build_ring(4)
    g = topology_to_graph(topo)
    caps = topology_to_capacities(topo)
    comms = all_pairs_commodities(g)
    rates = weighted_maxmin(comms, caps)
    for c in comms:
        src, dst = c["id"].split("->")
        rev = f"{dst}->{src}"
        assert abs(rates[c["id"]] - rates[rev]) < 1e-6, (
            f"asymmetric rates {c['id']}={rates[c['id']]} vs {rev}={rates[rev]}"
        )


def test_mesh_2x2_rates_positive_and_capacity_respected() -> None:
    topo = build_mesh(2, 2)
    g = topology_to_graph(topo)
    caps = topology_to_capacities(topo)
    comms = all_pairs_commodities(g)
    assert len(comms) == 4 * 3  # ordered pairs
    rates = weighted_maxmin(comms, caps)
    for r in rates.values():
        assert r > 0.0
    assert _capacity_respected(comms, caps, rates)


def test_star_2_branches_2_per_branch() -> None:
    # 1 center + 2 branches * 2 nodes = 5 nodes, 20 ordered pairs
    topo = build_star(per_branch_n=2, branches=2)
    g = topology_to_graph(topo)
    caps = topology_to_capacities(topo)
    comms = all_pairs_commodities(g)
    assert len(comms) == 5 * 4
    rates = weighted_maxmin(comms, caps)
    for r in rates.values():
        assert r > 0.0
    assert _capacity_respected(comms, caps, rates)


# -----------------------------------------------------------------------------
# Determinism / reproducibility
# -----------------------------------------------------------------------------


def test_maxmin_is_deterministic() -> None:
    topo = build_ring(5)
    g = topology_to_graph(topo)
    caps = topology_to_capacities(topo)
    comms = all_pairs_commodities(g)
    r1 = weighted_maxmin(comms, caps)
    r2 = weighted_maxmin(comms, caps)
    assert r1 == r2


# -----------------------------------------------------------------------------
# Bottleneck shape
# -----------------------------------------------------------------------------


def test_single_bottleneck_two_parallel_flows() -> None:
    """One edge shared by two flows, two extra edges in parallel.

    Setup: graph a-b, c-b, with caps cap(a,b)=100 and cap(b,c)=100.
    Flow1: a->c via [a,b,c], cap-limited by both edges.
    Flow2: b->c via [b,c], cap-limited only by (b,c).
    Both compete on (b,c) but only Flow1 uses (a,b). The maxmin
    algorithm should grow both equally on (b,c) until it saturates,
    then Flow2 freezes; Flow1 has no other path so it also freezes.
    Final: both get 50 each (split of (b,c)).
    """
    caps = {("a", "b"): 100.0, ("b", "c"): 100.0}
    comms = [
        {"id": "f1", "path": ["a", "b", "c"]},
        {"id": "f2", "path": ["b", "c"]},
    ]
    r = weighted_maxmin(comms, caps)
    assert abs(r["f1"] - 50.0) < 1e-6
    assert abs(r["f2"] - 50.0) < 1e-6
