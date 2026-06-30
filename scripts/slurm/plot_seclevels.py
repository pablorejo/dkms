#!/usr/bin/env python3
"""Analyse the RAW security-level sweep data and render plots.

Reads data/topo_metrics.jsonl (fine topology sweep) and data/runtime_metrics.jsonl
(mined deployment logs); writes PNGs to plots/. Run after topo_sweep.py +
runtime_mine.py.
"""
import json
import os
import sys
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

BASE = sys.argv[1] if len(sys.argv) > 1 else \
    "/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust/tests/results/seclevels-campaign"
DATA = os.path.join(BASE, "data"); PLOTS = os.path.join(BASE, "plots")
os.makedirs(PLOTS, exist_ok=True)


def load(name):
    p = os.path.join(DATA, name)
    return [json.loads(l) for l in open(p)] if os.path.exists(p) else []


topo = load("topo_metrics.jsonl")
rt = load("runtime_metrics.jsonl")
COLORS = {"er": "#1f77b4", "ba": "#ff7f0e", "rgg": "#2ca02c"}
TOPOS = ["er", "ba", "rgg"]


def agg(rows, keyf, valf):
    """mean,std of valf grouped by keyf."""
    g = defaultdict(list)
    for r in rows:
        g[keyf(r)].append(valf(r))
    return {k: (np.mean(v), np.std(v)) for k, v in g.items()}


# ===== FIGURE 1: topology landscape (4 panels) =====
fig, ax = plt.subplots(2, 2, figsize=(13, 9))

# (a) QKD-grade reachability vs PQC fraction (N=30, per topo, mean±std over seeds)
for t in TOPOS:
    rows = [r for r in topo if r["topo"] == t and r["N"] == 30]
    a = agg(rows, lambda r: r["pqc"], lambda r: r["qkd_conn_frac"])
    xs = sorted(a); ms = [a[x][0] for x in xs]; sd = [a[x][1] for x in xs]
    ax[0, 0].plot(xs, ms, "-o", color=COLORS[t], label=t.upper(), ms=4)
    ax[0, 0].fill_between(xs, np.array(ms) - sd, np.array(ms) + sd, color=COLORS[t], alpha=0.15)
ax[0, 0].set(title="QKD-grade reachability vs PQC-link fraction (N=30)",
             xlabel="fraction of links that are PQC", ylabel="fraction of DKMS pairs\nserved QKD-grade (qkd_prefer)")
ax[0, 0].grid(alpha=0.3); ax[0, 0].legend(); ax[0, 0].set_ylim(-0.02, 1.02)

# (b) QKD-grade reachability vs N (er, per pqc level)
for pqc in [0.2, 0.4, 0.5, 0.6, 0.8]:
    rows = [r for r in topo if r["topo"] == "er" and abs(r["pqc"] - pqc) < 1e-6]
    a = agg(rows, lambda r: r["N"], lambda r: r["qkd_conn_frac"])
    xs = sorted(a); ms = [a[x][0] for x in xs]
    ax[0, 1].plot(xs, ms, "-o", label=f"pqc={pqc}", ms=4)
ax[0, 1].set(title="QKD-grade reachability vs N (ER)",
             xlabel="number of DKMS nodes (N)", ylabel="fraction QKD-grade")
ax[0, 1].grid(alpha=0.3); ax[0, 1].legend(fontsize=8); ax[0, 1].set_ylim(-0.02, 1.02)

# (c) QKD-subgraph components vs PQC fraction (N=30, per topo)
for t in TOPOS:
    rows = [r for r in topo if r["topo"] == t and r["N"] == 30]
    a = agg(rows, lambda r: r["pqc"], lambda r: r["qkd_components"])
    xs = sorted(a); ms = [a[x][0] for x in xs]
    ax[1, 0].plot(xs, ms, "-o", color=COLORS[t], label=t.upper(), ms=4)
ax[1, 0].set(title="QKD-subgraph fragmentation vs PQC fraction (N=30)",
             xlabel="fraction of links that are PQC", ylabel="# QKD-connected components\n(1 = fully QKD-reachable)")
ax[1, 0].grid(alpha=0.3); ax[1, 0].legend()

# (d) heatmap qkd_conn_frac over (N, pqc) for ER
Ns = sorted({r["N"] for r in topo if r["topo"] == "er"})
Pq = sorted({r["pqc"] for r in topo if r["topo"] == "er"})
M = np.zeros((len(Pq), len(Ns)))
for i, pq in enumerate(Pq):
    for j, n in enumerate(Ns):
        vs = [r["qkd_conn_frac"] for r in topo if r["topo"] == "er" and r["N"] == n and abs(r["pqc"] - pq) < 1e-6]
        M[i, j] = np.mean(vs) if vs else np.nan
im = ax[1, 1].imshow(M, aspect="auto", origin="lower", cmap="viridis", vmin=0, vmax=1,
                     extent=[min(Ns), max(Ns), 0, 1])
ax[1, 1].set(title="QKD-grade reachability heatmap (ER)", xlabel="N", ylabel="PQC fraction")
fig.colorbar(im, ax=ax[1, 1], label="fraction QKD-grade")
fig.suptitle("QKD/PQC security levels — topology landscape (mean over 8 seeds)", fontsize=13)
fig.tight_layout(rect=[0, 0, 1, 0.97])
f1 = os.path.join(PLOTS, "topology_landscape.png"); fig.savefig(f1, dpi=110); plt.close(fig)

# ===== FIGURE 2: runtime (2 panels) =====
fig2, ax2 = plt.subplots(1, 2, figsize=(13, 4.6))

# (a) SDN MCMCF solve time vs N
done = [r for r in rt if r.get("solve_ms")]
for t in TOPOS:
    pts = sorted([(r["N"], r["solve_ms"] / 1000) for r in done if r["topo"] == t])
    if pts:
        xs, ys = zip(*pts)
        ax2[0].plot(xs, ys, "o", color=COLORS[t], label=t.upper(), ms=7)
# mark N=100 failure
fail = [r for r in rt if r["N"] == 100 and not r.get("solve_ms")]
if fail:
    ax2[0].axvline(100, ls="--", color="red", alpha=0.6)
    ax2[0].text(100, ax2[0].get_ylim()[1] * 0.5 if False else 200, "N=100:\nno converge\nen 480s",
                color="red", ha="center", fontsize=8)
ax2[0].set(title="SDN MCMCF LP solve time vs N", xlabel="N", ylabel="solve time (s)", yscale="log")
ax2[0].grid(alpha=0.3, which="both"); ax2[0].legend()

# (b) lambda (achievable fill scale) vs PQC fraction, N=20 ER
pts = sorted([(r["pqc"], r["lam"]) for r in rt if r["topo"] == "er" and r["N"] == 20 and r.get("lam") is not None])
if pts:
    xs, ys = zip(*pts)
    ax2[1].plot(xs, ys, "-o", color=COLORS["er"], ms=6)
    ax2[1].set_yscale("log")
ax2[1].set(title="λ (fill scale factor) vs PQC fraction (ER N=20)",
           xlabel="PQC fraction", ylabel="λ  (log)")
ax2[1].annotate("all-PQC: λ explodes\n(PQC links uncapacitated,\n1e9 sentinel)",
                xy=(1.0, ys[-1]) if pts else (1, 1), xytext=(0.45, ys[-1] / 50 if pts else 1),
                fontsize=8, arrowprops=dict(arrowstyle="->", color="gray"))
ax2[1].grid(alpha=0.3, which="both")
fig2.suptitle("Runtime behaviour (measured on CESGA Slurm deployments)", fontsize=13)
fig2.tight_layout(rect=[0, 0, 1, 0.95])
f2 = os.path.join(PLOTS, "runtime_behaviour.png"); fig2.savefig(f2, dpi=110); plt.close(fig2)

print("wrote:", f1, f2)
