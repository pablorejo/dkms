"""Theoretical max-min model — Python port of ``sdn/src/mcf.rs``.

The Rust solver uses a two-tier hybrid (HIGH/LOW) with decade-spaced
QoS weights. For the **theoretical** model we assume **uniform weight 1.0**
per commodity (all flows belong to the same tier, no QoS skew). With
uniform weights, the hybrid collapses to a single ``weighted_maxmin``
pass on the full capacity — see ``sdn/src/mcf.rs::weighted_maxmin``.

This module exposes:

* ``shortest_path(graph, src, dst)`` — BFS single-path.
* ``path_edges(path)`` — node-path → list of canonical edge keys.
* ``weighted_maxmin(commodities, capacities, weights=None)`` —
  Python clone of the Rust progressive-filling algorithm.
* ``topology_to_graph(topo)`` — adjacency dict from a builder output.
* ``topology_to_capacities(topo, alpha=0.2)`` —
  C = R0 × 10^(-α·d/10) per edge. ``alpha=0.2`` by default to match the
  orchestator's effective behavior (it ignores ``alpha=0`` and applies
  0.2 internally — bug documented in ``CLAUDE.md`` and ``tests/cli/README.md``;
  rule R-011 in the agent restrictions).
* ``all_pairs_commodities(graph, nodes)`` — ordered (src!=dst) commodities
  with BFS shortest path.

Conventions:

* A **node** is a string uid (e.g. ``"node-1"``).
* A **path** is a list of node uids, ordered from source to destination
  (inclusive on both ends).
* An **edge key** is a sorted tuple ``(u, v)`` with ``u <= v`` — same
  canonical form used by ``topology_builders._link``.
* A **commodity** is a dict ``{"id": str, "path": list[str]}``.
* **capacities** is a dict ``edge_key -> capacity (float, > 0)``.
* **weights** is a dict ``commodity_id -> weight (float >= 0)``;
  defaults to 1.0 per commodity.
"""

from __future__ import annotations

import math
from collections import deque
from typing import Any, Iterable

# Default effective alpha applied by the orchestator (R-011 bug).
DEFAULT_EFFECTIVE_ALPHA: float = 0.2

EPS: float = 1e-9


# -----------------------------------------------------------------------------
# Graph helpers
# -----------------------------------------------------------------------------


def topology_to_graph(topo: dict[str, Any]) -> dict[str, list[str]]:
    """Convert a builder topology to an adjacency dict (undirected)."""
    adj: dict[str, list[str]] = {nd["uid"]: [] for nd in topo["nodes"]}
    for ln in topo["links"]:
        a, b = ln["source_uid"], ln["target_uid"]
        if b not in adj[a]:
            adj[a].append(b)
        if a not in adj[b]:
            adj[b].append(a)
    return adj


def topology_to_capacities(
    topo: dict[str, Any], alpha: float = DEFAULT_EFFECTIVE_ALPHA
) -> dict[tuple[str, str], float]:
    """Compute per-edge capacity ``C = R0 × 10^(-α·d/10)``.

    Defaults to ``alpha=0.2`` because the orchestator IGNORES ``alpha=0``
    and applies 0.2 internally (R-011). Override with ``alpha=0.0`` only
    if you know that bug was fixed.
    """
    caps: dict[tuple[str, str], float] = {}
    for ln in topo["links"]:
        a, b = ln["source_uid"], ln["target_uid"]
        key = (a, b) if a <= b else (b, a)
        r0 = float(ln["quditto_rate_r0"])
        d = float(ln["distance_km"])
        cap = r0 * math.pow(10.0, -alpha * d / 10.0)
        caps[key] = cap
    return caps


def shortest_path(
    graph: dict[str, list[str]], src: str, dst: str
) -> list[str] | None:
    """BFS shortest path from ``src`` to ``dst`` (inclusive).

    Returns the node sequence ``[src, ..., dst]`` or ``None`` if disconnected.
    Adjacency iteration follows insertion order in ``graph[u]``, so paths
    are deterministic given a deterministic builder.
    """
    if src == dst:
        return [src]
    if src not in graph or dst not in graph:
        return None
    visited = {src}
    parents: dict[str, str | None] = {src: None}
    q: deque[str] = deque([src])
    while q:
        u = q.popleft()
        for v in graph.get(u, ()):
            if v in visited:
                continue
            visited.add(v)
            parents[v] = u
            if v == dst:
                path: list[str] = [dst]
                cur: str | None = u
                while cur is not None:
                    path.append(cur)
                    cur = parents[cur]
                return list(reversed(path))
            q.append(v)
    return None


def path_edges(path: list[str]) -> list[tuple[str, str]]:
    """Convert a node path to a list of canonical edge keys."""
    out: list[tuple[str, str]] = []
    for i in range(len(path) - 1):
        a, b = path[i], path[i + 1]
        out.append((a, b) if a <= b else (b, a))
    return out


def all_pairs_commodities(
    graph: dict[str, list[str]], nodes: Iterable[str] | None = None
) -> list[dict[str, Any]]:
    """Build ordered (src!=dst) commodities with BFS shortest paths.

    Skips pairs that are disconnected (returns no commodity for them).
    """
    node_list = list(nodes) if nodes is not None else list(graph.keys())
    out: list[dict[str, Any]] = []
    for src in node_list:
        for dst in node_list:
            if src == dst:
                continue
            p = shortest_path(graph, src, dst)
            if p is None:
                continue
            out.append({"id": f"{src}->{dst}", "path": p})
    return out


# -----------------------------------------------------------------------------
# Weighted max-min progressive filling
# -----------------------------------------------------------------------------


def weighted_maxmin(
    commodities: list[dict[str, Any]],
    capacities: dict[tuple[str, str], float],
    weights: dict[str, float] | None = None,
) -> dict[str, float]:
    """Weighted max-min progressive filling.

    Returns ``{commodity_id: rate}`` for commodities with rate > 0.
    Mirrors the semantics of ``sdn/src/mcf.rs::weighted_maxmin``:

    1. Per edge, sum the weights of every active flow traversing it.
    2. Find the largest ``delta`` such that ``edge_w[e] * delta`` fits in
       ``remaining[e]`` for every edge.
    3. Grow all active flows by ``weight[i] * delta``. Subtract consumed
       capacity from each edge.
    4. Edges that hit zero remaining are saturated. Flows crossing any
       saturated edge are frozen (made inactive).
    5. Repeat until no active flow remains, or no edge can absorb more.

    Commodities with no path or zero/negative weight are inactive.
    Edges absent from ``capacities`` are silently ignored (treated as
    unbounded, same as the Rust impl).
    """
    if not commodities or not capacities:
        return {}

    edge_list = sorted(capacities.keys())
    edge_idx = {ek: i for i, ek in enumerate(edge_list)}
    n_edges = len(edge_list)
    n_flows = len(commodities)

    flow_edges: list[list[int]] = [[] for _ in range(n_flows)]
    for i, c in enumerate(commodities):
        for ek in path_edges(c.get("path", [])):
            idx = edge_idx.get(ek)
            if idx is not None:
                flow_edges[i].append(idx)

    weights = weights or {}
    flow_w = [float(weights.get(c["id"], 1.0)) for c in commodities]
    caps = [float(capacities[ek]) for ek in edge_list]

    rates = [0.0] * n_flows
    remaining = caps.copy()
    active = [flow_w[i] > 0.0 and bool(flow_edges[i]) for i in range(n_flows)]

    guard = 0
    while True:
        guard += 1
        if guard > n_edges + 2:
            break

        edge_w = [0.0] * n_edges
        for i in range(n_flows):
            if not active[i]:
                continue
            w = flow_w[i]
            for e in flow_edges[i]:
                edge_w[e] += w

        delta = math.inf
        for e in range(n_edges):
            if edge_w[e] > EPS and remaining[e] > EPS:
                d = remaining[e] / edge_w[e]
                if d < delta:
                    delta = d
        if not math.isfinite(delta) or delta <= EPS:
            break

        for i in range(n_flows):
            if active[i]:
                rates[i] += flow_w[i] * delta
        for e in range(n_edges):
            if edge_w[e] > EPS:
                remaining[e] -= edge_w[e] * delta
                if remaining[e] < EPS:
                    remaining[e] = 0.0

        saturated = {e for e in range(n_edges) if remaining[e] <= EPS}
        if not saturated:
            break
        for i in range(n_flows):
            if not active[i]:
                continue
            if any(e in saturated for e in flow_edges[i]):
                active[i] = False
        if not any(active):
            break

    return {commodities[i]["id"]: rates[i] for i in range(n_flows) if rates[i] > 0.0}


# Alias matching ``.objetives.md`` naming.
maxmin = weighted_maxmin


__all__ = [
    "DEFAULT_EFFECTIVE_ALPHA",
    "EPS",
    "all_pairs_commodities",
    "maxmin",
    "path_edges",
    "shortest_path",
    "topology_to_capacities",
    "topology_to_graph",
    "weighted_maxmin",
]
