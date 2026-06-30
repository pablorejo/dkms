#!/usr/bin/env python3
"""Draw the QKD/PQC topologies used in the campaign.

Nodes at their builder (x,y); edges coloured by link type (QKD = solid blue,
PQC = dashed orange); nodes coloured by QKD-connected component (so the
fragmentation under qkd_prefer is visible). Same PQC-assignment rule as
gen_deploy (--pqc-fraction = longest fraction by distance).
"""
import os
import sys

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import matplotlib.cm as cm  # noqa: E402
import numpy as np  # noqa: E402

REPO = "/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust"
sys.path.insert(0, os.path.join(REPO, "tests/cli"))
import topology_builders as tb  # noqa: E402

OUT = os.path.join(REPO, "tests/results/seclevels-campaign/plots/topologies.png")
os.makedirs(os.path.dirname(OUT), exist_ok=True)


def build(topo, n, seed):
    if topo == "er":
        return tb.build_er(n, 4.0, seed)
    if topo == "ba":
        return tb.build_barabasi_albert(n, 4.0, seed)
    if topo == "rgg":
        return tb.build_rgg(n, 30.0, 4.0, seed)
    raise ValueError(topo)


def nid(uid):
    return int(uid.split("-")[1])


def analyse(g, pqc_frac):
    pos = {nid(nd["uid"]): (nd["x"], nd["y"]) for nd in g["nodes"]}
    edges = []
    for ln in g["links"]:
        a, b = nid(ln["source_uid"]), nid(ln["target_uid"])
        lo, hi = (a, b) if a <= b else (b, a)
        edges.append((lo, hi, float(ln.get("distance_km", 5.0))))
    n_pqc = int(pqc_frac * len(edges))
    order = sorted(edges, key=lambda e: (-e[2], e[0], e[1]))
    pqc = {(lo, hi) for lo, hi, _ in order[:n_pqc]}
    # QKD components
    adj = {x: set() for x in pos}
    for lo, hi, _ in edges:
        if (lo, hi) not in pqc:
            adj[lo].add(hi); adj[hi].add(lo)
    comp = {}
    nc = 0
    for x in pos:
        if x in comp:
            continue
        st = [x]; comp[x] = nc
        while st:
            y = st.pop()
            for z in adj[y]:
                if z not in comp:
                    comp[z] = nc; st.append(z)
        nc += 1
    return pos, edges, pqc, comp, nc


def draw(ax, topo, n, seed, pqc_frac):
    g = build(topo, n, seed)
    pos, edges, pqc, comp, nc = analyse(g, pqc_frac)
    cmap = cm.get_cmap("tab20", max(nc, 1))
    # edges
    for lo, hi, _ in edges:
        x = [pos[lo][0], pos[hi][0]]; y = [pos[lo][1], pos[hi][1]]
        if (lo, hi) in pqc:
            ax.plot(x, y, color="#ff7f0e", lw=1.6, ls="--", alpha=0.85, zorder=1)
        else:
            ax.plot(x, y, color="#1f77b4", lw=1.4, alpha=0.7, zorder=1)
    # nodes coloured by QKD component
    xs = [pos[k][0] for k in pos]; ys = [pos[k][1] for k in pos]
    cs = [cmap(comp[k]) for k in pos]
    ax.scatter(xs, ys, c=cs, s=90, edgecolors="black", linewidths=0.6, zorder=2)
    nq = len(edges) - len(pqc)
    ax.set_title(f"{topo.upper()}  N={n}  pqc={pqc_frac}\n"
                 f"{nq} QKD / {len(pqc)} PQC edges · {nc} QKD comps",
                 fontsize=10)
    ax.set_xticks([]); ax.set_yticks([])
    ax.set_aspect("equal", adjustable="datalim")


# 2 rows (pqc 0.3, 0.7) x 3 cols (er,ba,rgg), N=20, campaign seeds
fig, axes = plt.subplots(2, 3, figsize=(14, 9.2))
SEEDS = {"er": 11, "ba": 12, "rgg": 13}
for i, pqc in enumerate([0.3, 0.7]):
    for j, topo in enumerate(["er", "ba", "rgg"]):
        draw(axes[i, j], topo, 20, SEEDS[topo], pqc)
# legend
from matplotlib.lines import Line2D
fig.legend(handles=[Line2D([0], [0], color="#1f77b4", lw=2, label="QKD link"),
                    Line2D([0], [0], color="#ff7f0e", lw=2, ls="--", label="PQC link"),
                    Line2D([0], [0], marker="o", color="w", markerfacecolor="gray",
                           markeredgecolor="k", markersize=9, label="node colour = QKD component")],
           loc="lower center", ncol=3, fontsize=10)
fig.suptitle("Topologías de prueba — enlaces QKD vs PQC y fragmentación del subgrafo QKD",
             fontsize=13)
fig.tight_layout(rect=[0, 0.04, 1, 0.96])
fig.savefig(OUT, dpi=115)
print("wrote", OUT)
