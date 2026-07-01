#!/usr/bin/env python3
"""Analyse the RAW PQC-fraction saturation-ramp campaign and render plots.

Reads each cell dir <base>/er_n30_pqc<F>_s<S>/ produced by pqc_ramp_campaign.sh:
  plan.json               (meta.edge_list, meta.pqc_edges -> QKD subgraph)
  sae_ramp_by_level.csv   (SAE-indexed knee / throughput / latency, analyze_sae_ramp.py)
  requests.csv            (merged per-request raw, with master_host_id/slave_host_id)

Per cell we recover: the saturation knee (#SAEs cleanly served), the max served
throughput (keys/s), latency at the knee, the QKD-connected-pair fraction (the
mechanism), and a per-GRADE breakdown (each round-trip pair classified QKD-grade
if its endpoints share a QKD component, else PQC-grade). Output PNGs to
<base>/plots/. Pure stdlib + matplotlib(Agg). Analysis is separate from the run.

Usage: plot_pqc_ramp.py <campaign-dir>
"""
from __future__ import annotations

import bisect
import csv
import glob
import json
import os
import re
import sys
from collections import defaultdict

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import numpy as np  # noqa: E402

BASE = sys.argv[1] if len(sys.argv) > 1 else \
    "/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust/tests/results/pqc-ramp-er-n30"
PLOTS = os.path.join(BASE, "plots")
os.makedirs(PLOTS, exist_ok=True)

MATCH_THRESH = 99.5   # % round-trip match to count a level "cleanly served"
H429_THRESH = 0.5     # % 429 to count a level "cleanly served"
CELL_RE = re.compile(r"_pqc([0-9.]+)_s(\d+)$")


# ───────────────────────── topology / grade helpers ─────────────────────────
def qkd_components(edge_list, pqc_edges, ids):
    """Connected components of the QKD subgraph (all edges minus PQC edges)."""
    pqc = {tuple(e) for e in pqc_edges}
    adj = {int(x): set() for x in ids}
    for a, b in edge_list:
        lo, hi = (a, b) if a <= b else (b, a)
        if (lo, hi) in pqc:
            continue
        adj[a].add(b)
        adj[b].add(a)
    comp = {}
    cid = 0
    for x in adj:
        if x in comp:
            continue
        stack = [x]
        comp[x] = cid
        while stack:
            y = stack.pop()
            for z in adj[y]:
                if z not in comp:
                    comp[z] = cid
                    stack.append(z)
        cid += 1
    return comp


def qkd_conn_frac(comp, ids):
    """Fraction of ordered DKMS pairs that are QKD-connected (same component)."""
    from collections import Counter
    csize = Counter(comp.values())
    conn = sum(s * (s - 1) for s in csize.values())
    n = len(ids)
    tot = n * (n - 1)
    return (conn / tot) if tot else 0.0


# ───────────────────────────── per-cell loaders ─────────────────────────────
def knee_and_throughput(by_level_csv):
    """(knee SAEs, max served keys/s, p50@knee, p90@knee, p99@knee, censored)."""
    rows = []
    with open(by_level_csv, newline="") as fh:
        for d in csv.DictReader(fh):
            rows.append({k: float(v) for k, v in d.items()})
    rows.sort(key=lambda r: r["sae_level"])
    # cold-start / SDN-degenerate: the very first ramp level is already failing
    # (buffers never primed because the SDN allocated ~0 fill). Not real saturation.
    degenerate = bool(rows) and rows[0]["match_pct"] < 50.0
    knee = 0.0
    knee_row = None
    broke = False
    for r in rows:
        clean = (r["match_pct"] >= MATCH_THRESH) and (r["http429_pct"] <= H429_THRESH)
        if clean and not broke:
            knee = r["sae_level"]
            knee_row = r
        elif not clean:
            broke = True
    # max served throughput = max over levels of req/s * fraction of enc that succeeded
    max_thru = max((r["thru_rps"] * r["ok_enc_pct"] / 100.0) for r in rows) if rows else 0.0
    censored = not broke  # never saturated within the ramp -> knee is a lower bound
    if knee_row is None:  # never clean (cold start / broken)
        return knee, max_thru, float("nan"), float("nan"), float("nan"), censored, degenerate
    return (knee, max_thru, knee_row["rt_p50_ms"], knee_row["rt_p90_ms"],
            knee_row["rt_p99_ms"], censored, degenerate)


def per_grade(requests_csv, comp):
    """Per-grade aggregate over the requests of one cell.

    Returns {grade: {n, match_pct, h429_pct, keys_per_s, p50, p90, p99, n_pairs}}.
    grade: 'qkd' if (master_host_id, slave_host_id) share a QKD component, else 'pqc'.
    """
    g = {"qkd": defaultdict(float), "pqc": defaultdict(float)}
    lat = {"qkd": [], "pqc": []}
    pairs = {"qkd": set(), "pqc": set()}
    tmin = {"qkd": float("inf"), "pqc": float("inf")}
    tmax = {"qkd": float("-inf"), "pqc": float("-inf")}
    with open(requests_csv, newline="") as fh:
        for d in csv.DictReader(fh):
            try:
                a = int(d["master_host_id"])
                b = int(d["slave_host_id"])
                t = float(d["t_emit"])
            except (ValueError, KeyError):
                continue
            grade = "qkd" if comp.get(a, -1) == comp.get(b, -2) else "pqc"
            s = g[grade]
            s["n"] += 1
            s["match"] += int(d.get("match") or 0)
            s["ok_enc"] += int(d.get("ok_enc") or 0)
            err = d.get("err") or ""
            if err.startswith("enc HTTP 429") or err.startswith("dec HTTP 429"):
                s["h429"] += 1
            pairs[grade].add((d.get("worker_id"), d.get("pair_id")))
            tmin[grade] = min(tmin[grade], t)
            tmax[grade] = max(tmax[grade], t)
            if int(d.get("match") or 0):
                lat[grade].append(float(d.get("enc_ms") or 0) + float(d.get("dec_ms") or 0))
    out = {}
    for grade in ("qkd", "pqc"):
        n = g[grade]["n"]
        if n == 0:
            out[grade] = None
            continue
        dur = max(1e-6, tmax[grade] - tmin[grade])
        s = sorted(lat[grade])

        def p(pp):
            if not s:
                return float("nan")
            k = (len(s) - 1) * pp / 100.0
            lo = int(k)
            hi = min(lo + 1, len(s) - 1)
            return s[lo] + (s[hi] - s[lo]) * (k - lo)
        out[grade] = {
            "n": n,
            "match_pct": 100.0 * g[grade]["match"] / n,
            "h429_pct": 100.0 * g[grade]["h429"] / n,
            "keys_per_s": g[grade]["ok_enc"] / dur,
            "p50": p(50), "p90": p(90), "p99": p(99),
            "n_pairs": len(pairs[grade]),
        }
    return out


def per_grade_ramp(requests_csv, comp, grid=200):
    """Per-grade match%/429% indexed by active-SAE level (for one cell)."""
    rows = []
    first_emit = {}
    with open(requests_csv, newline="") as fh:
        for d in csv.DictReader(fh):
            try:
                a = int(d["master_host_id"])
                b = int(d["slave_host_id"])
                t = float(d["t_emit"])
            except (ValueError, KeyError):
                continue
            gkey = (d.get("worker_id"), d.get("pair_id"))
            if gkey not in first_emit or t < first_emit[gkey]:
                first_emit[gkey] = t
            grade = "qkd" if comp.get(a, -1) == comp.get(b, -2) else "pqc"
            rows.append((t, grade, int(d.get("match") or 0),
                         (d.get("err") or "").startswith(("enc HTTP 429", "dec HTTP 429"))))
    if not rows:
        return {}
    acts = sorted(first_emit.values())

    def active(t):
        return 2 * bisect.bisect_right(acts, t)
    agg = defaultdict(lambda: defaultdict(float))
    for t, grade, m, is429 in rows:
        lvl = int(round(active(t) / grid) * grid)
        a = agg[(grade, lvl)]
        a["n"] += 1
        a["match"] += m
        a["h429"] += 1 if is429 else 0
    out = {"qkd": [], "pqc": []}
    for (grade, lvl), a in sorted(agg.items(), key=lambda kv: kv[0][1]):
        out[grade].append((lvl, 100.0 * a["match"] / a["n"], 100.0 * a["h429"] / a["n"]))
    return out


# ─────────────────────── control-plane (SDN) loaders ────────────────────────
def _node(s):
    """'dkms-7.log'->7, 'dkms-7'->7."""
    m = re.search(r"dkms-(\d+)", str(s))
    return int(m.group(1)) if m else -1


_ANSI = re.compile(r"\x1b\[[0-9;]*m")
_GS = re.compile(r"peer=(dkms-\d+) .*?sdn_rate_keys_per_s=\"([0-9.]+)\"")


def load_sdn_rates(cell_dir, comp):
    """MAX SDN-assigned fill rate (keys/s/buffer) overall and per grade.

    Parses the per-cell copied dkms-*.log (full generator.state history) and takes
    the MAX sdn_rate per (src,peer) buffer over the run — the last snapshot reads
    ~0 when buffers are full (no fill needed), so it understates the capacity. A
    buffer (src->peer) is QKD-grade if src,peer share a QKD component, else PQC.
    The SDN rate is the achievable key-rate capacity the MCMCF allocates — where
    the PQC fraction shows up (PQC arcs uncapacitated via the 1e9 sentinel).
    """
    mx = {}  # (src,dst) -> max sdn_rate
    for f in glob.glob(os.path.join(cell_dir, "dkms-*.log")):
        src = _node(os.path.basename(f))
        with open(f, errors="ignore") as fh:
            for ln in fh:
                if "generator.state" not in ln:
                    continue
                m = _GS.search(_ANSI.sub("", ln))
                if not m:
                    continue
                dst = _node(m.group(1))
                r = float(m.group(2))
                k = (src, dst)
                if r > mx.get(k, -1.0):
                    mx[k] = r
    if not mx:
        return {}
    qkd, pqc = [], []
    for (src, dst), r in mx.items():
        (qkd if comp.get(src, -1) == comp.get(dst, -2) else pqc).append(r)
    allr = list(mx.values())
    med = lambda xs: float(np.median(xs)) if xs else float("nan")
    # aggregate capacity: median per-buffer max * total #buffers (N*(N-1))
    n = len([x for x in comp]) if comp else 0
    return {"sdn_all": med(allr), "sdn_qkd": med(qkd), "sdn_pqc": med(pqc),
            "n_buf_sampled": len(allr), "agg": med(allr) * n * (n - 1) if n else float("nan")}


def load_lambda(path):
    """MAX lambda over the run + its flows_with_rate (last snapshot reads ~0 when
    buffers are full; the peak is the achievable global fill scale)."""
    if not os.path.exists(path):
        return {}
    best = None
    for ln in open(path):
        m = re.search(r"lambda=([0-9.eE+-]+) flows_with_rate=(\d+)", ln)
        if m:
            lam = float(m.group(1))
            if best is None or lam > best[0]:
                best = (lam, int(m.group(2)))
    return {"lambda": best[0], "flows_with_rate": best[1]} if best else {}


# ───────────────────────────── load all cells ───────────────────────────────
cells = []
for d in sorted(glob.glob(os.path.join(BASE, "er_n30_pqc*_s*"))):
    m = CELL_RE.search(os.path.basename(d))
    if not m:
        continue
    plan_p = os.path.join(d, "plan.json")
    lvl_p = os.path.join(d, "sae_ramp_by_level.csv")
    req_p = os.path.join(d, "requests.csv")
    if not (os.path.exists(plan_p) and os.path.exists(lvl_p)):
        print(f"skip {os.path.basename(d)}: missing plan.json/sae_ramp_by_level.csv")
        continue
    pqc = float(m.group(1))
    seed = int(m.group(2))
    plan = json.load(open(plan_p))
    meta = plan["meta"]
    ids = [int(x) for x in plan["ids"]]
    comp = qkd_components(meta["edge_list"], meta["pqc_edges"], ids)
    knee, thru, p50, p90, p99, censored, degenerate = knee_and_throughput(lvl_p)
    rec = dict(dir=d, pqc=pqc, seed=seed, knee=knee, thru=thru,
               p50=p50, p90=p90, p99=p99, censored=censored, degenerate=degenerate,
               qkd_frac=qkd_conn_frac(comp, ids),
               n_pqc=len(meta["pqc_edges"]), n_edges=meta["edges"])
    rec["grade"] = per_grade(req_p, comp) if (os.path.exists(req_p) and not degenerate) else None
    rec.update(load_sdn_rates(d, comp))   # control plane (max sdn_rate from copied dkms-*.log)
    rec.update(load_lambda(os.path.join(d, "sdn_lambda.txt")))
    rec["comp"] = comp
    cells.append(rec)

if not cells:
    raise SystemExit(f"no analysable cells under {BASE}")
print(f"loaded {len(cells)} cells")
PQCS = sorted({c["pqc"] for c in cells})


def by_pqc(valf, skip_degen=True):
    """mean,std of valf over seeds, grouped by pqc; skips NaN (and degenerate
    SDN-cold-start cells by default — they are not real data-plane behaviour)."""
    g = defaultdict(list)
    for c in cells:
        if skip_degen and c.get("degenerate"):
            continue
        v = valf(c)
        if v is not None and not (isinstance(v, float) and np.isnan(v)):
            g[c["pqc"]].append(v)
    xs = sorted(g)
    return xs, [np.mean(g[x]) for x in xs], [np.std(g[x]) for x in xs]


n_degen = sum(1 for c in cells if c.get("degenerate"))
if n_degen:
    print(f"NOTE: {n_degen} SDN-degenerate cell(s) excluded from data-plane plots: "
          + ", ".join(f"pqc={c['pqc']}" for c in cells if c.get("degenerate")))


# ═══════ FIGURE 0 (HEADLINE): SDN-allocated key-rate capacity vs PQC ═════════
# The PQC effect lives in the control plane: the SDN MCMCF assigns a fill-rate
# capacity per (src,dst) commodity. PQC arcs are uncapacitated (1e9 sentinel) so
# as the PQC fraction rises the achievable key-rate explodes, while QKD-grade
# commodities stay capacity-limited. (The data plane doesn't enforce per-grade
# fill yet, so this capacity is what COULD be served, not what one node drives.)
fig0, ax0 = plt.subplots(2, 2, figsize=(13, 9))

x, m, s = by_pqc(lambda c: c.get("sdn_all"))
ax0[0, 0].errorbar(x, m, yerr=s, fmt="-o", color="#9467bd", capsize=3, ms=5)
ax0[0, 0].set(title="SDN-assigned fill rate (mediana) vs fracción PQC",
              xlabel="fracción de enlaces PQC", ylabel="keys/s por buffer (SDN)")
ax0[0, 0].set_yscale("symlog"); ax0[0, 0].grid(alpha=0.3, which="both")

for grade, col, lab in [("sdn_qkd", "#1f77b4", "buffers QKD-grade"),
                        ("sdn_pqc", "#ff7f0e", "buffers PQC-grade")]:
    x, m, s = by_pqc(lambda c, k=grade: c.get(k))
    if x:
        ax0[0, 1].plot(x, m, "-o", color=col, ms=5, label=lab)
ax0[0, 1].set(title="SDN fill rate por GRADO de buffer vs fracción PQC",
              xlabel="fracción de enlaces PQC", ylabel="keys/s por buffer (SDN)")
ax0[0, 1].set_yscale("symlog"); ax0[0, 1].legend(fontsize=9); ax0[0, 1].grid(alpha=0.3, which="both")

x, m, s = by_pqc(lambda c: c.get("lambda"))
ax0[1, 0].errorbar(x, m, yerr=s, fmt="-o", color="#2ca02c", capsize=3, ms=5)
ax0[1, 0].set(title="λ del MCMCF (escala global de llenado) vs fracción PQC",
              xlabel="fracción de enlaces PQC", ylabel="λ")
ax0[1, 0].set_yscale("symlog"); ax0[1, 0].grid(alpha=0.3, which="both")

x, m, s = by_pqc(lambda c: c.get("agg"))
ax0[1, 1].errorbar(x, m, yerr=s, fmt="-o", color="#d62728", capsize=3, ms=5)
ax0[1, 1].set(title="Capacidad agregada de claves del SDN vs fracción PQC",
              xlabel="fracción de enlaces PQC", ylabel="Σ keys/s (todos los buffers)")
ax0[1, 1].set_yscale("symlog"); ax0[1, 1].grid(alpha=0.3, which="both")

fig0.suptitle("Plano de CONTROL — capacidad de key-rate que asigna el SDN MCMCF "
              "(el efecto PQC real)", fontsize=13)
fig0.tight_layout(rect=[0, 0, 1, 0.96])
f0 = os.path.join(PLOTS, "sdn_capacity_vs_pqc.png"); fig0.savefig(f0, dpi=120); plt.close(fig0)


# ═══════════════════ FIGURE 1: saturation vs PQC fraction ═══════════════════
fig, ax = plt.subplots(2, 2, figsize=(13, 9))

x, m, s = by_pqc(lambda c: c["knee"])
ax[0, 0].errorbar(x, m, yerr=s, fmt="-o", color="#1f77b4", capsize=3, ms=5)
# mark right-censored cells (never saturated -> knee is a lower bound)
cx = [c["pqc"] for c in cells if c["censored"]]
cy = [c["knee"] for c in cells if c["censored"]]
if cx:
    ax[0, 0].scatter(cx, cy, marker="^", color="#1f77b4", edgecolor="k",
                     zorder=5, label="no saturó (cota inferior)")
    ax[0, 0].legend(fontsize=8)
ax[0, 0].set(title="Knee de saturación vs fracción PQC",
             xlabel="fracción de enlaces PQC", ylabel="SAEs servidos limpiamente\n(match≥99.5%, 429≤0.5%)")
ax[0, 0].grid(alpha=0.3)

x, m, s = by_pqc(lambda c: c["thru"])
ax[0, 1].errorbar(x, m, yerr=s, fmt="-o", color="#2ca02c", capsize=3, ms=5)
ax[0, 1].set(title="Throughput máx servido vs fracción PQC",
             xlabel="fracción de enlaces PQC", ylabel="claves/s servidas (máx en la rampa)")
ax[0, 1].grid(alpha=0.3)

# mechanism: qkd_conn_frac is a TOPOLOGY quantity (no SDN solve) so include ALL
# cells, even SDN-degenerate ones (marked) — this is the clean headline signal.
x, m, s = by_pqc(lambda c: c["qkd_frac"], skip_degen=False)
ax[1, 0].errorbar(x, m, yerr=s, fmt="-o", color="#d62728", capsize=3, ms=5)
dgx = [c["pqc"] for c in cells if c.get("degenerate")]
dgy = [c["qkd_frac"] for c in cells if c.get("degenerate")]
if dgx:
    ax[1, 0].scatter(dgx, dgy, marker="x", s=80, color="black", zorder=6,
                     label="SDN-degenerado (LP)")
    ax[1, 0].legend(fontsize=8)
ax[1, 0].set(title="Mecanismo: fracción de pares QKD-conexos vs fracción PQC",
             xlabel="fracción de enlaces PQC", ylabel="fracción de pares servidos QKD-grade\n(qkd_prefer)")
ax[1, 0].set_ylim(-0.02, 1.02)
ax[1, 0].grid(alpha=0.3)

for lbl, key, col in [("p50", "p50", "#1f77b4"), ("p90", "p90", "#ff7f0e"), ("p99", "p99", "#d62728")]:
    x, m, s = by_pqc(lambda c, k=key: c[k])
    ax[1, 1].errorbar(x, m, yerr=s, fmt="-o", color=col, capsize=2, ms=4, label=lbl)
ax[1, 1].set(title="Latencia round-trip en el knee vs fracción PQC",
             xlabel="fracción de enlaces PQC", ylabel="latencia enc+dec (ms)")
ax[1, 1].legend(); ax[1, 1].grid(alpha=0.3)

fig.suptitle("Rampas de saturación ER N=30 — barrido de fracción PQC (enlaces PQC aleatorios)",
             fontsize=13)
fig.tight_layout(rect=[0, 0, 1, 0.96])
f1 = os.path.join(PLOTS, "saturation_vs_pqc.png"); fig.savefig(f1, dpi=120); plt.close(fig)


# ═══════════════════ FIGURE 2: ramp curves overlaid per PQC ═════════════════
# one representative seed per pqc (the lowest seed available)
rep = {}
for c in sorted(cells, key=lambda c: c["seed"]):
    if c.get("degenerate"):
        continue
    rep.setdefault(c["pqc"], c)
fig2, (axa, axb) = plt.subplots(2, 1, figsize=(11, 9), sharex=True)
cmap = plt.cm.viridis
for c in sorted(rep.values(), key=lambda c: c["pqc"]):
    rows = []
    with open(os.path.join(c["dir"], "sae_ramp_by_level.csv"), newline="") as fh:
        for d in csv.DictReader(fh):
            rows.append((float(d["sae_level"]), float(d["match_pct"]), float(d["http429_pct"])))
    rows.sort()
    xs = [r[0] for r in rows]
    col = cmap(c["pqc"])
    axa.plot(xs, [r[1] for r in rows], "-o", color=col, ms=3, label=f"pqc={c['pqc']:.1f}")
    axb.plot(xs, [r[2] for r in rows], "-s", color=col, ms=3)
axa.axhline(MATCH_THRESH, ls="--", lw=0.8, color="gray", alpha=0.6)
axa.set(title="Curvas de rampa por fracción PQC (1 semilla representativa)",
        ylabel="round-trip match %"); axa.set_ylim(-2, 102)
axa.legend(fontsize=8, ncol=2, loc="lower left"); axa.grid(alpha=0.3)
axb.axhline(H429_THRESH, ls="--", lw=0.8, color="gray", alpha=0.6)
axb.set(xlabel="SAEs activos", ylabel="429 backpressure %"); axb.grid(alpha=0.3)
fig2.tight_layout()
f2 = os.path.join(PLOTS, "ramp_curves_by_pqc.png"); fig2.savefig(f2, dpi=120); plt.close(fig2)


# ═══════════════ FIGURE 3: per-GRADE breakdown vs PQC (the extra) ════════════
def grade_series(grade, field):
    g = defaultdict(list)
    for c in cells:
        gr = c.get("grade") or {}
        if gr.get(grade):
            g[c["pqc"]].append(gr[grade][field])
    xs = sorted(g)
    return xs, [np.mean(g[x]) for x in xs], [np.std(g[x]) for x in xs]


fig3, ax3 = plt.subplots(2, 2, figsize=(13, 9))
GC = {"qkd": "#1f77b4", "pqc": "#ff7f0e"}
GL = {"qkd": "QKD-grade (par QKD-conexo)", "pqc": "PQC-grade (par QKD-inconexo)"}
for grade in ("qkd", "pqc"):
    x, m, s = grade_series(grade, "match_pct")
    ax3[0, 0].errorbar(x, m, yerr=s, fmt="-o", color=GC[grade], capsize=2, ms=4, label=GL[grade])
    x, m, s = grade_series(grade, "h429_pct")
    ax3[0, 1].errorbar(x, m, yerr=s, fmt="-o", color=GC[grade], capsize=2, ms=4, label=GL[grade])
    x, m, s = grade_series(grade, "keys_per_s")
    ax3[1, 0].errorbar(x, m, yerr=s, fmt="-o", color=GC[grade], capsize=2, ms=4, label=GL[grade])
    x, m, s = grade_series(grade, "p99")
    ax3[1, 1].errorbar(x, m, yerr=s, fmt="-o", color=GC[grade], capsize=2, ms=4, label=GL[grade])
ax3[0, 0].set(title="match % por grado vs fracción PQC", xlabel="fracción PQC", ylabel="match %")
ax3[0, 0].set_ylim(-2, 102)
ax3[0, 1].set(title="429 backpressure % por grado vs fracción PQC", xlabel="fracción PQC", ylabel="429 %")
ax3[1, 0].set(title="claves/s servidas por grado vs fracción PQC", xlabel="fracción PQC", ylabel="claves/s")
ax3[1, 1].set(title="latencia p99 por grado vs fracción PQC", xlabel="fracción PQC", ylabel="p99 (ms)")
for a in ax3.flat:
    a.legend(fontsize=8); a.grid(alpha=0.3)
fig3.suptitle("Desglose por GRADO bajo carga — el tráfico servido se desplaza de QKD-grade "
              "a PQC-grade al subir PQC (ambos ~100% match, ~0% 429)", fontsize=12)
fig3.tight_layout(rect=[0, 0, 1, 0.96])
f3 = os.path.join(PLOTS, "per_grade_vs_pqc.png"); fig3.savefig(f3, dpi=120); plt.close(fig3)


# ═══════════ FIGURE 4: per-grade ramp curves for a mid PQC cell ══════════════
mid = min(rep.values(), key=lambda c: abs(c["pqc"] - 0.5)) if rep else None
f4 = None
if mid and os.path.exists(os.path.join(mid["dir"], "requests.csv")):
    pg = per_grade_ramp(os.path.join(mid["dir"], "requests.csv"), mid["comp"])
    if pg.get("qkd") or pg.get("pqc"):
        fig4, (a1, a2) = plt.subplots(2, 1, figsize=(11, 9), sharex=True)
        for grade in ("qkd", "pqc"):
            if not pg.get(grade):
                continue
            xs = [r[0] for r in pg[grade]]
            a1.plot(xs, [r[1] for r in pg[grade]], "-o", color=GC[grade], ms=3, label=GL[grade])
            a2.plot(xs, [r[2] for r in pg[grade]], "-s", color=GC[grade], ms=3, label=GL[grade])
        a1.axhline(MATCH_THRESH, ls="--", lw=0.8, color="gray", alpha=0.6)
        a1.set(title=f"Rampa por grado — celda pqc={mid['pqc']:.1f} "
                     f"(qkd-conn-frac={mid['qkd_frac']:.2f})", ylabel="match %")
        a1.set_ylim(-2, 102); a1.legend(fontsize=9); a1.grid(alpha=0.3)
        a2.set(xlabel="SAEs activos", ylabel="429 %"); a2.legend(fontsize=9); a2.grid(alpha=0.3)
        fig4.tight_layout()
        f4 = os.path.join(PLOTS, "per_grade_ramp_mid.png"); fig4.savefig(f4, dpi=120); plt.close(fig4)


# ───────────────────────────── summary table ────────────────────────────────
summ = os.path.join(BASE, "summary_by_pqc.csv")
with open(summ, "w", newline="") as fh:
    w = csv.writer(fh)
    w.writerow(["pqc", "seed", "knee_saes", "censored", "max_keys_per_s", "qkd_conn_frac",
                "sdn_rate_all", "sdn_rate_qkd", "sdn_rate_pqc", "sdn_agg", "lambda",
                "p50_knee_ms", "p99_knee_ms",
                "qkd_n", "qkd_match%", "qkd_429%", "qkd_keys_s",
                "pqc_n", "pqc_match%", "pqc_429%", "pqc_keys_s"])
    rnd = lambda v: round(v, 2) if isinstance(v, (int, float)) and not np.isnan(v) else ""
    for c in sorted(cells, key=lambda c: (c["pqc"], c["seed"])):
        gr = c.get("grade") or {}
        q = gr.get("qkd") or {}
        p = gr.get("pqc") or {}
        w.writerow([c["pqc"], c["seed"], int(c["knee"]), int(c["censored"]),
                    round(c["thru"], 1), round(c["qkd_frac"], 3),
                    rnd(c.get("sdn_all", float("nan"))), rnd(c.get("sdn_qkd", float("nan"))),
                    rnd(c.get("sdn_pqc", float("nan"))), rnd(c.get("agg", float("nan"))),
                    rnd(c.get("lambda", float("nan"))),
                    round(c["p50"], 1) if not np.isnan(c["p50"]) else "",
                    round(c["p99"], 1) if not np.isnan(c["p99"]) else "",
                    q.get("n", 0), round(q.get("match_pct", float("nan")), 1),
                    round(q.get("h429_pct", float("nan")), 2), round(q.get("keys_per_s", 0), 1),
                    p.get("n", 0), round(p.get("match_pct", float("nan")), 1),
                    round(p.get("h429_pct", float("nan")), 2), round(p.get("keys_per_s", 0), 1)])

print("wrote:")
for f in [f0, f1, f2, f3, f4, summ]:
    if f:
        print("  ", f)
