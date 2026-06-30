#!/usr/bin/env python3
"""Collect RAW topology metrics for the QKD/PQC security-level sweep.

For each (topology, N, pqc_fraction, seed) it builds the graph with the same
`tests/cli/topology_builders` used by gen_deploy, marks the longest
`pqc_fraction` of edges as PQC (same rule as gen_deploy --pqc-fraction), and
computes how the QKD subgraph fragments. Pure computation — no deployment — so
the sweep can be fine-grained. Writes one JSON object per line to the output
file; analysis/plots are a separate step (plot_seclevels.py).

Metrics per row: edges (total/qkd/pqc), qkd_components, qkd_connected_pairs,
total_pairs, qkd_conn_frac (= fraction of ordered DKMS pairs that get QKD-grade
under qkd_prefer), mean_qkd_comp_size, full_connected.
"""
import json
import os
import sys

REPO = "/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust"
sys.path.insert(0, os.path.join(REPO, "tests/cli"))
import topology_builders as tb  # noqa: E402

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
    REPO, "tests/results/seclevels-campaign/data/topo_metrics.jsonl")
os.makedirs(os.path.dirname(OUT), exist_ok=True)


def build(topo, n, deg, seed):
    if topo == "er":
        return tb.build_er(n, deg, seed)
    if topo == "ba":
        return tb.build_barabasi_albert(n, deg, seed)
    if topo == "rgg":
        return tb.build_rgg(n, 30.0, deg, seed)
    if topo == "ring":
        return tb.build_ring(n)
    raise ValueError(topo)


def nid(uid):
    return int(uid.split("-")[1])


def metrics(g, pqc_frac):
    links = g["links"]
    # canonical int edge list with distance
    edges = []
    for ln in links:
        a, b = nid(ln["source_uid"]), nid(ln["target_uid"])
        lo, hi = (a, b) if a <= b else (b, a)
        edges.append((lo, hi, float(ln.get("distance_km", 5.0))))
    n_edges = len(edges)
    # gen_deploy rule: longest fraction by (-distance, lo, hi) becomes PQC
    order = sorted(edges, key=lambda e: (-e[2], e[0], e[1]))
    n_pqc = int(pqc_frac * n_edges)
    pqc = {(lo, hi) for lo, hi, _ in order[:n_pqc]}
    nodes = {nid(nd["uid"]) for nd in g["nodes"]}
    # QKD-only adjacency
    adj = {x: set() for x in nodes}
    for lo, hi, _ in edges:
        if (lo, hi) not in pqc:
            adj[lo].add(hi); adj[hi].add(lo)
    # components (union-find via BFS) over QKD edges
    comp = {}
    sizes = []
    for x in nodes:
        if x in comp:
            continue
        cid = len(sizes); sz = 0
        st = [x]; comp[x] = cid
        while st:
            y = st.pop(); sz += 1
            for z in adj[y]:
                if z not in comp:
                    comp[z] = cid; st.append(z)
        sizes.append(sz)
    # ordered DKMS pairs that are QKD-connected (same component)
    from collections import Counter
    csize = Counter(comp.values())
    qkd_conn_pairs = sum(s * (s - 1) for s in csize.values())  # ordered, within-comp
    total_pairs = len(nodes) * (len(nodes) - 1)
    # full-graph connectivity
    fadj = {x: set() for x in nodes}
    for lo, hi, _ in edges:
        fadj[lo].add(hi); fadj[hi].add(lo)
    seen = set(); st = [next(iter(nodes))]; seen.add(st[0])
    while st:
        y = st.pop()
        for z in fadj[y]:
            if z not in seen:
                seen.add(z); st.append(z)
    return {
        "n_nodes": len(nodes), "edges": n_edges, "qkd_edges": n_edges - len(pqc),
        "pqc_edges": len(pqc), "qkd_components": len(sizes),
        "qkd_connected_pairs": qkd_conn_pairs, "total_pairs": total_pairs,
        "qkd_conn_frac": (qkd_conn_pairs / total_pairs) if total_pairs else 0.0,
        "mean_qkd_comp_size": sum(sizes) / len(sizes) if sizes else 0,
        "max_qkd_comp_size": max(sizes) if sizes else 0,
        "full_connected": len(seen) == len(nodes),
    }


TOPOS = ["er", "ba", "rgg"]
NS = [10, 15, 20, 25, 30, 40, 50, 60, 80, 100]
PQCS = [round(x / 10, 1) for x in range(0, 11)]
SEEDS = list(range(1, 9))
DEG = 4.0

rows = 0
with open(OUT, "w") as fh:
    for topo in TOPOS:
        for n in NS:
            for seed in SEEDS:
                try:
                    g = build(topo, n, DEG, seed)
                except Exception as e:  # noqa: BLE001
                    print(f"skip {topo} n={n} seed={seed}: {e}")
                    continue
                for pqc in PQCS:
                    m = metrics(g, pqc)
                    m.update(topo=topo, N=n, pqc=pqc, seed=seed, degree=DEG)
                    fh.write(json.dumps(m) + "\n")
                    rows += 1
print(f"wrote {rows} rows -> {OUT}")
