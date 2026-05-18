"""Topology builders for the ``dkms-topo`` CLI.

Each builder returns a dict ``{"nodes": [...], "links": [...]}`` compatible
with the orchestator schema (``WebNodeInput`` / ``WebLinkInput`` in
``orchestrator/api_orchestator.py``). The dict is the **topology fragment**;
``sim_payload.build_sim_payload`` wraps it with ``sdn``, ``name``,
``description`` to obtain the full request body.

Defaults chosen to match the dkms1 EKS deployment:

- ``link_type = "QKD"``
- ``distance_km = 5``
- ``quditto_rate_r0 = 2000.0``
- ``quditto_rate_alpha = 0.0`` — the orchestator ignores ``alpha=0`` and
  applies ``0.2`` internally (bug documented in ``tests/cli/README.md``,
  rule R-011 in the agent restrictions). The theoretical model should
  therefore use ``alpha=0.2`` even if this default is ``0.0``.
- ``quditto_max_buffer_size = 65536`` — matches the env overrides in
  ``orchestator.py`` for sidecar deployments.

Connectivity is guaranteed for every builder. ``build_random`` uses a
random spanning tree as a seed and then densifies with additional random
edges until ``avg_degree`` is met.
"""

from __future__ import annotations

import math
import random as _random
from collections import deque
from typing import Any

DEFAULT_R0: float = 2000.0
DEFAULT_ALPHA: float = 0.0
DEFAULT_DISTANCE_KM: int = 5
DEFAULT_BUFFER_SIZE: int = 65_536
DEFAULT_LINK_TYPE: str = "QKD"


def _node(node_id: int, x: float, y: float) -> dict[str, Any]:
    return {
        "uid": f"node-{node_id}",
        "node_id": node_id,
        "label": f"DKMS {node_id}",
        "x": float(x),
        "y": float(y),
        "enc_buffer_size": None,
        "dec_buffer_size": None,
    }


def _link(src_uid: str, dst_uid: str) -> dict[str, Any]:
    left, right = (src_uid, dst_uid) if src_uid <= dst_uid else (dst_uid, src_uid)
    return {
        "uid": f"edge-{left}-{right}",
        "source_uid": left,
        "target_uid": right,
        "link_type": DEFAULT_LINK_TYPE,
        "distance_km": DEFAULT_DISTANCE_KM,
        "quditto_rate_r0": DEFAULT_R0,
        "quditto_rate_alpha": DEFAULT_ALPHA,
        "quditto_max_buffer_size": DEFAULT_BUFFER_SIZE,
    }


def _ensure_connected(
    nodes: list[dict[str, Any]], links: list[dict[str, Any]]
) -> bool:
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


def build_ring(n: int) -> dict[str, Any]:
    """N-node ring topology. Requires ``n >= 3`` to form a proper cycle."""
    if n < 3:
        raise ValueError(f"ring requires n >= 3, got {n}")
    nodes: list[dict[str, Any]] = []
    radius = 220.0
    cx, cy = 320.0, 220.0
    for i in range(n):
        angle = (2.0 * math.pi * i) / n
        nodes.append(
            _node(i + 1, cx + radius * math.cos(angle), cy + radius * math.sin(angle))
        )
    links: list[dict[str, Any]] = []
    for i in range(n):
        a = nodes[i]["uid"]
        b = nodes[(i + 1) % n]["uid"]
        links.append(_link(a, b))
    return {"nodes": nodes, "links": links}


def build_line(n: int) -> dict[str, Any]:
    """N-node line (path) topology. Requires ``n >= 2``."""
    if n < 2:
        raise ValueError(f"line requires n >= 2, got {n}")
    nodes: list[dict[str, Any]] = []
    step = 120.0
    x0 = 100.0
    y = 220.0
    for i in range(n):
        nodes.append(_node(i + 1, x0 + step * i, y))
    links: list[dict[str, Any]] = []
    for i in range(n - 1):
        links.append(_link(nodes[i]["uid"], nodes[i + 1]["uid"]))
    return {"nodes": nodes, "links": links}


def build_mesh(n: int, m: int) -> dict[str, Any]:
    """Rectangular ``n × m`` grid (4-neighbour mesh). Requires ``n>=2``, ``m>=2``."""
    if n < 2 or m < 2:
        raise ValueError(f"mesh requires n>=2 and m>=2, got n={n}, m={m}")
    nodes: list[dict[str, Any]] = []
    step = 120.0
    x0, y0 = 100.0, 100.0
    grid: list[list[dict[str, Any]]] = [
        [{} for _ in range(m)] for _ in range(n)
    ]
    nid = 1
    for r in range(n):
        for c in range(m):
            nd = _node(nid, x0 + step * c, y0 + step * r)
            grid[r][c] = nd
            nodes.append(nd)
            nid += 1
    links: list[dict[str, Any]] = []
    for r in range(n):
        for c in range(m):
            if c + 1 < m:
                links.append(_link(grid[r][c]["uid"], grid[r][c + 1]["uid"]))
            if r + 1 < n:
                links.append(_link(grid[r][c]["uid"], grid[r + 1][c]["uid"]))
    return {"nodes": nodes, "links": links}


def build_star(per_branch_n: int, branches: int) -> dict[str, Any]:
    """Star with one center node and ``branches`` arms of ``per_branch_n`` nodes each.

    Total node count = ``1 + per_branch_n * branches``.
    Requires ``per_branch_n >= 1`` and ``branches >= 2``.
    """
    if per_branch_n < 1:
        raise ValueError(f"star requires per_branch_n >= 1, got {per_branch_n}")
    if branches < 2:
        raise ValueError(f"star requires branches >= 2, got {branches}")
    nodes: list[dict[str, Any]] = []
    cx, cy = 320.0, 220.0
    center = _node(1, cx, cy)
    nodes.append(center)
    nid = 2
    links: list[dict[str, Any]] = []
    radius_step = 100.0
    for b in range(branches):
        angle = (2.0 * math.pi * b) / branches
        prev_uid = center["uid"]
        for k in range(per_branch_n):
            r = radius_step * (k + 1)
            x = cx + r * math.cos(angle)
            y = cy + r * math.sin(angle)
            nd = _node(nid, x, y)
            nodes.append(nd)
            links.append(_link(prev_uid, nd["uid"]))
            prev_uid = nd["uid"]
            nid += 1
    return {"nodes": nodes, "links": links}


def build_random(
    n: int, avg_degree: float, seed: int | None = None
) -> dict[str, Any]:
    """Connected random graph with target average degree.

    Starts from a random spanning tree (n-1 edges, guaranteed connected),
    then adds random edges until the total reaches ``ceil(n * avg_degree / 2)``.
    Requires ``n >= 2`` and ``2.0 <= avg_degree <= n - 1`` (avg_degree >= 2
    so the densification can produce a non-trivial graph beyond the tree).
    """
    if n < 2:
        raise ValueError(f"random requires n >= 2, got {n}")
    if avg_degree < 2.0:
        raise ValueError(
            f"random requires avg_degree >= 2.0 for non-trivial graphs, got {avg_degree}"
        )
    if avg_degree > n - 1:
        raise ValueError(
            f"random avg_degree {avg_degree} exceeds maximum n-1={n-1}"
        )
    rng = _random.Random(seed)
    nodes: list[dict[str, Any]] = []
    cx, cy = 320.0, 220.0
    radius = 260.0
    for i in range(n):
        angle = (2.0 * math.pi * i) / n
        nodes.append(
            _node(i + 1, cx + radius * math.cos(angle), cy + radius * math.sin(angle))
        )
    uids = [nd["uid"] for nd in nodes]
    rng.shuffle(uids)
    links: list[dict[str, Any]] = []
    edge_set: set[tuple[str, str]] = set()

    def add_edge(a: str, b: str) -> bool:
        if a == b:
            return False
        left, right = (a, b) if a <= b else (b, a)
        if (left, right) in edge_set:
            return False
        edge_set.add((left, right))
        links.append(_link(left, right))
        return True

    for i in range(1, len(uids)):
        prev = uids[rng.randrange(i)]
        add_edge(uids[i], prev)
    target_edges = int(math.ceil(n * avg_degree / 2.0))
    safety = 0
    max_safety = 20 * target_edges + 100
    while len(links) < target_edges and safety < max_safety:
        safety += 1
        a = uids[rng.randrange(n)]
        b = uids[rng.randrange(n)]
        add_edge(a, b)
    if not _ensure_connected(nodes, links):
        raise RuntimeError("random builder failed to produce a connected graph")
    return {"nodes": nodes, "links": links}


__all__ = [
    "build_ring",
    "build_line",
    "build_mesh",
    "build_star",
    "build_random",
    "DEFAULT_R0",
    "DEFAULT_ALPHA",
    "DEFAULT_DISTANCE_KM",
    "DEFAULT_BUFFER_SIZE",
    "DEFAULT_LINK_TYPE",
]
