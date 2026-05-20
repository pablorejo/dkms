"""Deep-analysis plots on saturation data — full set for the final report.

Reads:
  <output_dir>/data/generator_state.csv
  <output_dir>/data/per_commodity.csv

Emits ~10-12 plots in <output_dir>/plots/deep_*.png.
"""
from __future__ import annotations
import argparse, csv, math
from collections import defaultdict
from pathlib import Path
from datetime import datetime
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def _read(path: Path) -> list[dict]:
    if not path.exists():
        return []
    with path.open() as f:
        return list(csv.DictReader(f))


def _f(x, default=0.0):
    try: return float(x)
    except: return default


def _i(x, default=0):
    try: return int(float(x))
    except: return default


def _parse_iso(s):
    if not s: return None
    try: return datetime.fromisoformat(s.replace("Z", "+00:00"))
    except: return None


def deep_rate_histogram(pc, plots):
    rates = [_f(r["last_observed_keys_per_s"]) for r in pc if _f(r.get("last_observed_keys_per_s")) > 0]
    if not rates: return None
    fig, ax = plt.subplots(figsize=(11, 5))
    ax.hist(rates, bins=40, edgecolor="black", alpha=0.7, color="#3498db")
    ax.axvline(np.mean(rates), color="r", ls="--", label=f"mean={np.mean(rates):.1f}")
    ax.axvline(np.median(rates), color="g", ls="--", label=f"median={np.median(rates):.1f}")
    ax.set_xlabel("observed_keys_per_s")
    ax.set_ylabel("commodity count")
    ax.set_title(f"Distribution of observed rate per commodity (n={len(rates)})")
    ax.legend(); ax.grid(True, alpha=0.3)
    out = plots/"deep_rate_histogram.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_rate_by_src_box(pc, plots):
    by_src = defaultdict(list)
    for r in pc:
        v = _f(r["last_observed_keys_per_s"])
        if v > 0: by_src[r["src"]].append(v)
    if not by_src: return None
    srcs = sorted(by_src)
    data = [by_src[s] for s in srcs]
    fig, ax = plt.subplots(figsize=(14, 6))
    bp = ax.boxplot(data, tick_labels=srcs, showmeans=True, patch_artist=True)
    for p in bp["boxes"]: p.set_facecolor("#a8e6cf")
    ax.set_ylabel("observed_keys_per_s")
    ax.set_title("Observed rate per source DKMS (boxplot over its peers)")
    ax.tick_params(axis="x", labelrotation=90, labelsize=8)
    ax.grid(True, alpha=0.3, axis="y")
    out = plots/"deep_rate_by_src_box.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_t_first_emit_heatmap(pc, plots):
    srcs = sorted({r["src"] for r in pc})
    peers = sorted({r["peer"] for r in pc})
    si = {s:i for i,s in enumerate(srcs)}; pi = {p:i for i,p in enumerate(peers)}
    M = np.full((len(srcs), len(peers)), np.nan)
    t0 = None
    for r in pc:
        t = _parse_iso(r.get("t_first_emit", ""))
        if t is None: continue
        if t0 is None or t < t0: t0 = t
    for r in pc:
        t = _parse_iso(r.get("t_first_emit", ""))
        if t is None or t0 is None: continue
        M[si[r["src"]], pi[r["peer"]]] = (t - t0).total_seconds()
    fig, ax = plt.subplots(figsize=(13, 11))
    im = ax.imshow(M, cmap="plasma", aspect="auto")
    ax.set_xticks(range(len(peers))); ax.set_xticklabels(peers, rotation=90, fontsize=7)
    ax.set_yticks(range(len(srcs))); ax.set_yticklabels(srcs, fontsize=7)
    ax.set_xlabel("peer DKMS"); ax.set_ylabel("source DKMS")
    ax.set_title("Time-to-first-emit (s) per (src, peer) — measures bootstrap latency")
    fig.colorbar(im, ax=ax, label="seconds since first global emit")
    out = plots/"deep_t_first_emit_heatmap.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_t_sat_kde(pc, plots):
    ts = [_f(r["t_observed_seconds"]) for r in pc
          if r.get("saturated","").lower()=="true" and _f(r.get("t_observed_seconds")) > 0]
    if not ts: return None
    arr = np.array(ts)
    fig, ax = plt.subplots(figsize=(11, 5))
    # histogram + KDE estimate
    ax.hist(arr, bins=40, density=True, alpha=0.4, color="#9b59b6", label="histogram")
    # simple Gaussian KDE
    from math import erf
    sigma = arr.std()
    xs = np.linspace(arr.min()-2, arr.max()+2, 200)
    bw = 1.06 * sigma * (len(arr) ** -0.2)  # Silverman
    if bw > 0:
        kde = np.array([np.mean(np.exp(-((xs[i] - arr) / bw) ** 2 / 2) / (bw * np.sqrt(2 * np.pi))) for i in range(len(xs))])
        ax.plot(xs, kde, color="#e74c3c", lw=2, label=f"KDE (bw={bw:.2f})")
    ax.axvline(arr.mean(), color="r", ls=":", alpha=0.6)
    ax.set_xlabel("t_saturated (s)"); ax.set_ylabel("density")
    ax.set_title(f"Distribution of saturation time ({len(arr)} commodities, σ={sigma:.2f}s)")
    ax.legend(); ax.grid(True, alpha=0.3)
    out = plots/"deep_t_sat_kde.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_global_throughput(gs, plots):
    # Total keys/s emitted across all commodities over time
    by_t = defaultdict(float)
    counts = defaultdict(int)
    for r in gs:
        t = round(_f(r["t_seconds"]))
        rate = _f(r.get("observed_keys_per_s"))
        by_t[t] += rate
        counts[t] += 1
    if not by_t: return None
    ts = sorted(by_t)
    rates = [by_t[t] for t in ts]
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.plot(ts, rates, lw=2, color="#16a085")
    ax.fill_between(ts, 0, rates, alpha=0.2, color="#16a085")
    ax.set_xlabel("seconds since first emit")
    ax.set_ylabel("aggregate keys/s across all commodities")
    ax.set_title("Total network throughput over time")
    ax.grid(True, alpha=0.3)
    out = plots/"deep_global_throughput.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_commodities_state(gs, plots, buffer_size=4096, threshold=0.95):
    # 3-state stacked area: idle / filling / saturated, over time
    sat_th = threshold * buffer_size
    by_t = defaultdict(lambda: {"idle":0, "filling":0, "sat":0})
    for r in gs:
        t = round(_f(r["t_seconds"]))
        enc = _i(r.get("enc"))
        if enc == 0: by_t[t]["idle"] += 1
        elif enc >= sat_th: by_t[t]["sat"] += 1
        else: by_t[t]["filling"] += 1
    if not by_t: return None
    ts = sorted(by_t)
    idle = [by_t[t]["idle"] for t in ts]
    fill = [by_t[t]["filling"] for t in ts]
    sat = [by_t[t]["sat"] for t in ts]
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.stackplot(ts, idle, fill, sat,
                 labels=["idle (enc=0)", "filling", f"saturated (>{threshold:.0%})"],
                 colors=["#bdc3c7", "#f39c12", "#27ae60"], alpha=0.85)
    ax.set_xlabel("seconds since first emit"); ax.set_ylabel("# commodities")
    ax.set_title("Commodity state distribution over time (3 states)")
    ax.legend(loc="upper left"); ax.grid(True, alpha=0.3)
    out = plots/"deep_commodities_state.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_fairness_jain(gs, plots):
    # Jain's fairness index over time: (sum)^2 / (n * sum_of_squares)
    by_t = defaultdict(list)
    for r in gs:
        t = round(_f(r["t_seconds"]))
        rate = _f(r.get("observed_keys_per_s"))
        if rate > 0: by_t[t].append(rate)
    if not by_t: return None
    ts = sorted(by_t)
    jain = []
    for t in ts:
        arr = np.array(by_t[t])
        denom = len(arr) * np.sum(arr**2)
        j = (np.sum(arr)**2) / denom if denom > 0 else 0
        jain.append(j)
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.plot(ts, jain, lw=2, color="#8e44ad")
    ax.axhline(1.0, color="g", ls="--", alpha=0.5, label="perfect fairness (Jain=1)")
    ax.set_xlabel("seconds since first emit"); ax.set_ylabel("Jain's fairness index")
    ax.set_ylim(0, 1.05); ax.set_title("Jain's fairness of observed rates over time")
    ax.legend(); ax.grid(True, alpha=0.3)
    out = plots/"deep_fairness_jain.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_max_enc_distribution(pc, plots):
    encs = [_i(r["max_enc"]) for r in pc]
    if not encs: return None
    fig, ax = plt.subplots(figsize=(11, 5))
    ax.hist(encs, bins=40, edgecolor="black", alpha=0.7, color="#e67e22")
    ax.set_xlabel("max_enc reached"); ax.set_ylabel("# commodities")
    ax.set_title(f"Distribution of max ENC buffer fill ({len(encs)} commodities)")
    ax.grid(True, alpha=0.3)
    out = plots/"deep_max_enc_hist.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_rate_convergence(gs, plots):
    # Per-commodity rate trajectory + global median to show convergence
    rows = defaultdict(list)
    for r in gs:
        k = f"{r['src']}->{r['peer']}"
        rows[k].append((_f(r["t_seconds"]), _f(r.get("observed_keys_per_s"))))
    fig, ax = plt.subplots(figsize=(14, 7))
    for k, traj in rows.items():
        traj.sort()
        ts = [t for t,_ in traj]; rs = [r for _,r in traj]
        ax.plot(ts, rs, alpha=0.15, lw=0.6, color="#3498db")
    # median band
    by_t = defaultdict(list)
    for k, traj in rows.items():
        for t, r in traj:
            if r > 0: by_t[round(t)].append(r)
    ts = sorted(by_t)
    medians = [np.median(by_t[t]) for t in ts]
    ax.plot(ts, medians, lw=3, color="#c0392b", label="median across commodities")
    ax.set_xlabel("seconds since first emit"); ax.set_ylabel("observed_keys_per_s per commodity")
    ax.set_title("Per-commodity rate trajectory + median (rate convergence)")
    ax.legend(); ax.grid(True, alpha=0.3)
    out = plots/"deep_rate_convergence.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_per_src_sat_with_rate(pc, plots):
    # Bar: per src DKMS show sat ratio + median rate
    by_src_sat = defaultdict(lambda: [0,0])  # [sat, total]
    by_src_rate = defaultdict(list)
    for r in pc:
        by_src_sat[r["src"]][1] += 1
        if r.get("saturated","").lower() == "true":
            by_src_sat[r["src"]][0] += 1
        v = _f(r["last_observed_keys_per_s"])
        if v > 0: by_src_rate[r["src"]].append(v)
    srcs = sorted(by_src_sat)
    sats = [by_src_sat[s][0] for s in srcs]
    totals = [by_src_sat[s][1] for s in srcs]
    rates_p50 = [np.median(by_src_rate[s]) if by_src_rate[s] else 0 for s in srcs]
    fig, (ax1, ax2) = plt.subplots(2, 1, figsize=(14, 9), sharex=True)
    x = np.arange(len(srcs))
    ax1.bar(x, totals, color="#ecf0f1", label="total peers", edgecolor="black")
    ax1.bar(x, sats, color="#27ae60", label="saturated")
    ax1.set_ylabel("# peer commodities"); ax1.legend(); ax1.grid(True, alpha=0.3, axis="y")
    ax1.set_title("Per-source: saturated/total + median observed rate")
    ax2.bar(x, rates_p50, color="#2980b9")
    ax2.set_ylabel("median rate (keys/s)"); ax2.set_xticks(x)
    ax2.set_xticklabels(srcs, rotation=90, fontsize=8)
    ax2.grid(True, alpha=0.3, axis="y")
    out = plots/"deep_per_src_sat_rate.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def deep_ack_pending_distribution(gs, plots):
    # Per-commodity max ack_pending over the run — shows backpressure hotspots
    by_k = defaultdict(int)
    for r in gs:
        k = f"{r['src']}->{r['peer']}"
        v = _i(r.get("ack_pending"))
        if v > by_k[k]: by_k[k] = v
    if not by_k: return None
    vals = list(by_k.values())
    fig, ax = plt.subplots(figsize=(11, 5))
    ax.hist(vals, bins=40, edgecolor="black", alpha=0.7, color="#c0392b")
    ax.set_xlabel("max ack_pending observed per commodity")
    ax.set_ylabel("# commodities")
    ax.set_title("Distribution of peak ack_pending (back-pressure indicator)")
    ax.grid(True, alpha=0.3)
    out = plots/"deep_ack_pending_hist.png"
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig); return out


def main(out_dir: str):
    base = Path(out_dir)
    data = base/"data"
    plots = base/"plots"
    plots.mkdir(exist_ok=True)
    pc = _read(data/"per_commodity.csv")
    gs = _read(data/"generator_state.csv")
    produced = []
    if pc:
        for fn in [deep_rate_histogram, deep_rate_by_src_box, deep_t_first_emit_heatmap,
                   deep_t_sat_kde, deep_max_enc_distribution, deep_per_src_sat_with_rate]:
            r = fn(pc, plots)
            if r: produced.append(r); print(r)
    if gs:
        for fn in [deep_global_throughput, deep_commodities_state, deep_fairness_jain,
                   deep_rate_convergence, deep_ack_pending_distribution]:
            r = fn(gs, plots)
            if r: produced.append(r); print(r)
    print(f"\n{len(produced)} deep plots generated")


if __name__ == "__main__":
    import sys
    main(sys.argv[1] if len(sys.argv) > 1 else "tests/results/eks-mcmcf-er20")
