"""Unit tests for ``tests/cli/topology_builders``.

Run from the repo root with::

    pytest tests/cli/test_topology_builders.py
"""

from __future__ import annotations

from collections import deque

import pytest

from tests.cli.topology_builders import (
    DEFAULT_ALPHA,
    DEFAULT_BUFFER_SIZE,
    DEFAULT_DISTANCE_KM,
    DEFAULT_LINK_TYPE,
    DEFAULT_R0,
    build_barabasi_albert,
    build_bridge,
    build_er,
    build_line,
    build_mesh,
    build_rgg,
    build_ring,
    build_secoqc,
    build_star,
)

REQUIRED_NODE_FIELDS = {"uid", "node_id", "label", "x", "y"}
REQUIRED_LINK_FIELDS = {
    "uid",
    "source_uid",
    "target_uid",
    "link_type",
    "distance_km",
    "quditto_rate_r0",
    "quditto_rate_alpha",
    "quditto_max_buffer_size",
}


def _is_connected(topo: dict) -> bool:
    nodes = topo["nodes"]
    links = topo["links"]
    if not nodes:
        return True
    uid_to_idx = {nd["uid"]: i for i, nd in enumerate(nodes)}
    adj: list[list[int]] = [[] for _ in nodes]
    for ln in links:
        a = uid_to_idx[ln["source_uid"]]
        b = uid_to_idx[ln["target_uid"]]
        adj[a].append(b)
        adj[b].append(a)
    visited = {0}
    q: deque[int] = deque([0])
    while q:
        u = q.popleft()
        for v in adj[u]:
            if v not in visited:
                visited.add(v)
                q.append(v)
    return len(visited) == len(nodes)


def _undirected_edge(ln: dict) -> tuple[str, str]:
    a, b = ln["source_uid"], ln["target_uid"]
    return (a, b) if a <= b else (b, a)


def _assert_fields(topo: dict) -> None:
    for nd in topo["nodes"]:
        missing = REQUIRED_NODE_FIELDS - set(nd.keys())
        assert not missing, f"node missing fields {missing}: {nd}"
        assert isinstance(nd["node_id"], int) and nd["node_id"] >= 1
        assert isinstance(nd["label"], str) and nd["label"]
        assert isinstance(nd["x"], float) and isinstance(nd["y"], float)
    for ln in topo["links"]:
        missing = REQUIRED_LINK_FIELDS - set(ln.keys())
        assert not missing, f"link missing fields {missing}: {ln}"
        assert ln["link_type"] in ("QKD", "PQC", "HYBRID")
        assert isinstance(ln["distance_km"], int) and ln["distance_km"] >= 0
        assert isinstance(ln["quditto_rate_r0"], float) and ln["quditto_rate_r0"] > 0
        assert (
            isinstance(ln["quditto_rate_alpha"], float)
            and ln["quditto_rate_alpha"] >= 0
        )
        assert (
            isinstance(ln["quditto_max_buffer_size"], int)
            and ln["quditto_max_buffer_size"] >= 1
        )


# -----------------------------------------------------------------------------
# Ring
# -----------------------------------------------------------------------------


def test_ring_4_counts_and_cycle() -> None:
    topo = build_ring(4)
    assert len(topo["nodes"]) == 4
    assert len(topo["links"]) == 4
    edges = {_undirected_edge(ln) for ln in topo["links"]}
    assert len(edges) == 4, "duplicated edges in ring"
    assert _is_connected(topo)
    _assert_fields(topo)


def test_ring_uids_unique_and_node_ids_sequential() -> None:
    topo = build_ring(6)
    uids = [nd["uid"] for nd in topo["nodes"]]
    node_ids = [nd["node_id"] for nd in topo["nodes"]]
    assert len(set(uids)) == 6
    assert node_ids == [1, 2, 3, 4, 5, 6]


def test_ring_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_ring(2)
    with pytest.raises(ValueError):
        build_ring(0)


# -----------------------------------------------------------------------------
# Line
# -----------------------------------------------------------------------------


def test_line_5_counts() -> None:
    topo = build_line(5)
    assert len(topo["nodes"]) == 5
    assert len(topo["links"]) == 4
    assert _is_connected(topo)
    _assert_fields(topo)


def test_line_no_cycle() -> None:
    topo = build_line(4)
    # tree property: edges == nodes - 1, and acyclic
    assert len(topo["links"]) == len(topo["nodes"]) - 1
    edges = {_undirected_edge(ln) for ln in topo["links"]}
    assert len(edges) == len(topo["links"])  # no duplicates


def test_line_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_line(1)
    with pytest.raises(ValueError):
        build_line(0)


# -----------------------------------------------------------------------------
# Mesh
# -----------------------------------------------------------------------------


def test_mesh_3x3_counts() -> None:
    topo = build_mesh(3, 3)
    assert len(topo["nodes"]) == 9
    # rectangular grid: row-edges (3 rows * (3-1) cols) + col-edges ((3-1) rows * 3 cols)
    assert len(topo["links"]) == 12
    assert _is_connected(topo)
    _assert_fields(topo)


def test_mesh_2x4_counts() -> None:
    topo = build_mesh(2, 4)
    assert len(topo["nodes"]) == 8
    # row-edges: 2*3=6, col-edges: 1*4=4 -> 10
    assert len(topo["links"]) == 10
    assert _is_connected(topo)


def test_mesh_no_duplicate_edges() -> None:
    topo = build_mesh(3, 4)
    edges = {_undirected_edge(ln) for ln in topo["links"]}
    assert len(edges) == len(topo["links"])


def test_mesh_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_mesh(1, 3)
    with pytest.raises(ValueError):
        build_mesh(3, 1)
    with pytest.raises(ValueError):
        build_mesh(0, 0)


# -----------------------------------------------------------------------------
# Star
# -----------------------------------------------------------------------------


def test_star_2x3_counts() -> None:
    topo = build_star(per_branch_n=2, branches=3)
    # center + 2*3 branch nodes = 7
    assert len(topo["nodes"]) == 7
    # 3 branches * 2 nodes per branch = 6 edges
    assert len(topo["links"]) == 6
    assert _is_connected(topo)
    _assert_fields(topo)


def test_star_1x4_counts() -> None:
    topo = build_star(per_branch_n=1, branches=4)
    # center + 1*4 = 5 nodes, 4 edges
    assert len(topo["nodes"]) == 5
    assert len(topo["links"]) == 4
    assert _is_connected(topo)


def test_star_center_uid_is_first() -> None:
    topo = build_star(per_branch_n=2, branches=2)
    assert topo["nodes"][0]["uid"] == "node-1"
    assert topo["nodes"][0]["node_id"] == 1


def test_star_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_star(per_branch_n=0, branches=3)
    with pytest.raises(ValueError):
        build_star(per_branch_n=2, branches=1)
    with pytest.raises(ValueError):
        build_star(per_branch_n=-1, branches=3)


# -----------------------------------------------------------------------------
# Random
# -----------------------------------------------------------------------------


def test_random_8_connected() -> None:
    topo = build_er(n=8, avg_degree=3.0, seed=42)
    assert len(topo["nodes"]) == 8
    # ER is stochastic: edges ~ Binomial(C(N,2), p), grado medio ≈ ⟨k⟩.
    # Just verify the count is in the right ballpark.
    n_edges = len(topo["links"])
    avg_k = 2.0 * n_edges / 8
    assert 1.5 <= avg_k <= 4.5, f"ER avg_k out of range: {avg_k}"
    assert _is_connected(topo)
    _assert_fields(topo)


def test_random_reproducible_with_seed() -> None:
    topo_a = build_er(n=10, avg_degree=3.0, seed=123)
    topo_b = build_er(n=10, avg_degree=3.0, seed=123)
    edges_a = {_undirected_edge(ln) for ln in topo_a["links"]}
    edges_b = {_undirected_edge(ln) for ln in topo_b["links"]}
    assert edges_a == edges_b


def test_random_different_seeds_differ_typically() -> None:
    topo_a = build_er(n=12, avg_degree=3.0, seed=1)
    topo_b = build_er(n=12, avg_degree=3.0, seed=999)
    edges_a = {_undirected_edge(ln) for ln in topo_a["links"]}
    edges_b = {_undirected_edge(ln) for ln in topo_b["links"]}
    # very unlikely identical for non-trivial graphs
    assert edges_a != edges_b


def test_random_no_self_loops_or_duplicates() -> None:
    topo = build_er(n=15, avg_degree=4.0, seed=7)
    edge_set = set()
    for ln in topo["links"]:
        assert ln["source_uid"] != ln["target_uid"]
        pair = _undirected_edge(ln)
        assert pair not in edge_set, f"duplicate edge {pair}"
        edge_set.add(pair)


def test_er_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_er(n=1, avg_degree=2.0)
    with pytest.raises(ValueError):
        build_er(n=5, avg_degree=0.0)
    with pytest.raises(ValueError):
        build_er(n=5, avg_degree=5.0)  # avg_degree > n-1


# -----------------------------------------------------------------------------
# Barabási–Albert
# -----------------------------------------------------------------------------


def test_ba_basic_shape_and_connectivity() -> None:
    topo = build_barabasi_albert(n=20, avg_degree=4.0, seed=7)
    assert len(topo["nodes"]) == 20
    assert _is_connected(topo), "BA must be connected (m_0 clique seed)"
    avg_k = 2.0 * len(topo["links"]) / 20
    # BA: expected average degree converges to 2m. For m=2, ⟨k⟩ ≈ 4.
    assert 2.0 <= avg_k <= 6.0, f"BA avg_degree out of range: {avg_k}"


def test_ba_stochastic_m_for_odd_avg_degree() -> None:
    # avg_degree=5 → m_floor=2, p_extra=0.5 → mix of 2 and 3 edges per new node.
    topo = build_barabasi_albert(n=50, avg_degree=5.0, seed=42)
    avg_k = 2.0 * len(topo["links"]) / 50
    # Larger N reduces variance; expect ⟨k⟩ ≈ 5 ± 1.
    assert 3.5 <= avg_k <= 6.5, f"BA stochastic m gave avg_k={avg_k}"


def test_ba_seed_reproducibility() -> None:
    a = build_barabasi_albert(n=15, avg_degree=4.0, seed=42)
    b = build_barabasi_albert(n=15, avg_degree=4.0, seed=42)
    assert a == b


def test_ba_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_barabasi_albert(n=1, avg_degree=2.0)
    with pytest.raises(ValueError):
        build_barabasi_albert(n=10, avg_degree=1.5)  # < 2 ⇒ m < 1
    with pytest.raises(ValueError):
        build_barabasi_albert(n=5, avg_degree=10.0)  # > n-1


# -----------------------------------------------------------------------------
# Random Geometric Graph
# -----------------------------------------------------------------------------


def test_rgg_basic_shape_and_connectivity() -> None:
    topo = build_rgg(n=20, max_distance_km=10.0, avg_degree=4.0, seed=42)
    assert len(topo["nodes"]) == 20
    assert _is_connected(topo)


def test_rgg_distance_per_edge_varies() -> None:
    # Use a higher target avg_degree so the geometric sampling alone
    # produces a connected graph (avoiding the connectivity fallback
    # which can add long-distance edges).
    topo = build_rgg(n=20, max_distance_km=20.0, avg_degree=6.0, seed=42)
    dists = sorted(ln["distance_km"] for ln in topo["links"])
    # In RGG distances are euclidean random — they must vary.
    assert dists[0] < dists[-1], "RGG must yield variable per-edge distances"
    # All geometric edges are bounded by max_distance_km. The
    # connectivity fallback may add a few edges beyond it; allow a
    # small tail past the cutoff.
    bounded = sum(1 for d in dists if d <= 20.0)
    assert bounded >= 0.7 * len(dists), (
        f"≥70% of RGG edges should respect max_distance; "
        f"got {bounded}/{len(dists)} bounded"
    )


def test_rgg_seed_reproducibility() -> None:
    a = build_rgg(n=15, max_distance_km=8.0, avg_degree=4.0, seed=42)
    b = build_rgg(n=15, max_distance_km=8.0, avg_degree=4.0, seed=42)
    assert a == b


def test_rgg_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_rgg(n=1, max_distance_km=5.0, avg_degree=2.0)
    with pytest.raises(ValueError):
        build_rgg(n=10, max_distance_km=0.0, avg_degree=4.0)
    with pytest.raises(ValueError):
        build_rgg(n=10, max_distance_km=5.0, avg_degree=0.0)


# -----------------------------------------------------------------------------
# SECOQC (partial mesh)
# -----------------------------------------------------------------------------


def test_secoqc_basic_shape_and_connectivity() -> None:
    topo = build_secoqc(n=10, avg_degree=4.0, seed=42)
    assert len(topo["nodes"]) == 10
    assert _is_connected(topo)
    avg_k = 2.0 * len(topo["links"]) / 10
    assert abs(avg_k - 4.0) < 0.5, f"secoqc avg_k out of range: {avg_k}"


def test_secoqc_ring_minimum() -> None:
    # avg_degree=2 → exactly the ring, no extra chords.
    topo = build_secoqc(n=8, avg_degree=2.0, seed=42)
    assert len(topo["links"]) == 8  # N edges in a ring


def test_secoqc_seed_reproducibility() -> None:
    a = build_secoqc(n=12, avg_degree=3.0, seed=42)
    b = build_secoqc(n=12, avg_degree=3.0, seed=42)
    assert a == b


def test_secoqc_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_secoqc(n=2, avg_degree=2.0)  # n < 3
    with pytest.raises(ValueError):
        build_secoqc(n=10, avg_degree=1.0)  # < 2
    with pytest.raises(ValueError):
        build_secoqc(n=5, avg_degree=10.0)  # > n-1


# -----------------------------------------------------------------------------
# Defaults sanity (R-011 compliance: alpha emitted is 0.0)
# -----------------------------------------------------------------------------


def test_default_link_values() -> None:
    topo = build_ring(4)
    ln = topo["links"][0]
    assert ln["link_type"] == DEFAULT_LINK_TYPE == "QKD"
    assert ln["distance_km"] == DEFAULT_DISTANCE_KM == 5
    assert ln["quditto_rate_r0"] == DEFAULT_R0 == 2000.0
    # R-011: emit alpha=0.0; orchestator applies 0.2 internally
    assert ln["quditto_rate_alpha"] == DEFAULT_ALPHA == 0.0
    assert ln["quditto_max_buffer_size"] == DEFAULT_BUFFER_SIZE == 65_536


def test_link_uid_is_canonical_ordering() -> None:
    topo = build_ring(5)
    for ln in topo["links"]:
        a, b = ln["source_uid"], ln["target_uid"]
        assert a <= b, f"link uids not in canonical order: {a} > {b}"
        assert ln["uid"] == f"edge-{a}-{b}"


# -----------------------------------------------------------------------------
# Bridge (obvious bottleneck topology — multi-cluster joined by single edges)
# -----------------------------------------------------------------------------


def test_bridge_2_clusters_of_4_counts() -> None:
    topo = build_bridge(cluster_n=4, cluster_count=2)
    _assert_fields(topo)
    # 2 rings of 4 nodes = 8 nodes total.
    assert len(topo["nodes"]) == 8
    # Each ring has 4 edges; plus 1 bridge edge between clusters.
    assert len(topo["links"]) == 4 + 4 + 1
    assert _is_connected(topo)


def test_bridge_3_clusters_of_5_counts() -> None:
    topo = build_bridge(cluster_n=5, cluster_count=3)
    _assert_fields(topo)
    assert len(topo["nodes"]) == 15
    # 3 rings × 5 edges + 2 bridge edges between consecutive clusters.
    assert len(topo["links"]) == 3 * 5 + 2
    assert _is_connected(topo)


def test_bridge_has_single_min_cut_edge_between_clusters() -> None:
    """The bridge topology's defining property: removing the bridge edges
    splits the graph into exactly ``cluster_count`` components."""
    topo = build_bridge(cluster_n=4, cluster_count=2)
    # The bridge edges are between clusters; identify them by the pair of
    # node_ids straddling cluster_n boundaries. Cluster 1 has node_ids
    # 1..cluster_n; cluster 2 has cluster_n+1..2*cluster_n; etc.
    cluster_of = lambda nd: (nd["node_id"] - 1) // 4
    uid_to_node = {nd["uid"]: nd for nd in topo["nodes"]}
    inter_cluster = [
        ln
        for ln in topo["links"]
        if cluster_of(uid_to_node[ln["source_uid"]])
        != cluster_of(uid_to_node[ln["target_uid"]])
    ]
    # Exactly cluster_count - 1 = 1 bridge edge for 2 clusters.
    assert len(inter_cluster) == 1


def test_bridge_validation_errors() -> None:
    with pytest.raises(ValueError):
        build_bridge(cluster_n=2, cluster_count=2)  # ring requires ≥3
    with pytest.raises(ValueError):
        build_bridge(cluster_n=4, cluster_count=1)  # need ≥2 clusters
    with pytest.raises(ValueError):
        build_bridge(cluster_n=4, cluster_count=2, intra_degree=1.0)


def test_bridge_uids_unique_and_sequential() -> None:
    topo = build_bridge(cluster_n=4, cluster_count=3)
    uids = [nd["uid"] for nd in topo["nodes"]]
    assert len(uids) == len(set(uids))
    node_ids = [nd["node_id"] for nd in topo["nodes"]]
    assert node_ids == list(range(1, len(node_ids) + 1))
