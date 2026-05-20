"""Extended plots for ER-20 (or any random topology) MCMCF-λ runs.

Reads:
  <output_dir>/data/generator_state.csv
  <output_dir>/data/per_commodity.csv
  <output_dir>/loadtest_results.json     (optional, from --sae-test)
  <output_dir>/plots/                    (where new PNGs land)

Outputs (in <output_dir>/plots/):
  ex_rate_heatmap.png         — observed rate matrix src × peer
  ex_t_sat_cdf.png            — CDF + histogram of saturation time
  ex_buffer_at_sat.png        — final enc per commodity (boxplot per src)
  ex_emit_band.png            — mean ± stdev of emit rate over time
  ex_ack_pending_timeline.png — heatmap commodity × time (ack_pending)
  ex_per_src_sat.png          — bar chart sat/total per src DKMS
  ex_sdn_vs_observed_scatter.png — observed vs SDN-dictated scatter
  ex_loadtest_429_timeline.png — req status over time (if --sae-test)
  ex_loadtest_latency_cdf.png   — latency CDF per status (if --sae-test)
"""
from __future__ import annotations

import json
import math
from collections import defaultdict
from pathlib import Path
from typing import Any

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def _read_csv(path: Path) -> list[dict[str, str]]:
    import csv
    if not path.exists():
        return []
    with path.open() as f:
        return list(csv.DictReader(f))


def _safe_float(x: str, default: float = 0.0) -> float:
    try:
        return float(x)
    except (ValueError, TypeError):
        return default


def _safe_int(x: str, default: int = 0) -> int:
    try:
        return int(float(x))
    except (ValueError, TypeError):
        return default


def heatmap_rate(per_commodity: list[dict], plots_dir: Path) -> Path:
    """Heatmap of last observed rate, rows=src, cols=peer."""
    srcs = sorted({r["src"] for r in per_commodity})
    peers = sorted({r["peer"] for r in per_commodity})
    src_idx = {s: i for i, s in enumerate(srcs)}
    peer_idx = {p: i for i, p in enumerate(peers)}
    M = np.full((len(srcs), len(peers)), np.nan)
    for r in per_commodity:
        i = src_idx[r["src"]]
        j = peer_idx[r["peer"]]
        M[i, j] = _safe_float(r.get("last_observed_keys_per_s", "0"))

    fig, ax = plt.subplots(figsize=(14, 12))
    im = ax.imshow(M, cmap="viridis", aspect="auto")
    ax.set_xticks(range(len(peers)))
    ax.set_xticklabels(peers, rotation=90, fontsize=7)
    ax.set_yticks(range(len(srcs)))
    ax.set_yticklabels(srcs, fontsize=7)
    ax.set_xlabel("peer DKMS")
    ax.set_ylabel("source DKMS")
    ax.set_title("Observed rate (keys/s) per (src, peer) — last sample")
    fig.colorbar(im, ax=ax, label="keys/s")
    out = plots_dir / "ex_rate_heatmap.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def t_sat_cdf(per_commodity: list[dict], plots_dir: Path) -> Path:
    """CDF + histogram of t_saturated seconds (from first emit)."""
    t_sats = [_safe_float(r["t_observed_seconds"]) for r in per_commodity if r.get("saturated", "").lower() == "true" and r.get("t_observed_seconds")]
    t_sats = [t for t in t_sats if t > 0]
    if not t_sats:
        return plots_dir / "ex_t_sat_cdf.png"
    arr = np.array(sorted(t_sats))
    fig, (ax_cdf, ax_hist) = plt.subplots(1, 2, figsize=(14, 5))
    # CDF
    y = np.arange(1, len(arr) + 1) / len(arr)
    ax_cdf.plot(arr, y, lw=2)
    ax_cdf.set_xlabel("t_saturated (seconds since first emit)")
    ax_cdf.set_ylabel("CDF")
    ax_cdf.set_title(f"CDF of t_saturated ({len(arr)} commodities)")
    ax_cdf.grid(True, alpha=0.3)
    ax_cdf.axvline(arr.mean(), color="r", ls="--", label=f"mean={arr.mean():.1f}s")
    ax_cdf.axvline(np.median(arr), color="g", ls="--", label=f"median={np.median(arr):.1f}s")
    ax_cdf.legend()
    # Histogram
    ax_hist.hist(arr, bins=30, edgecolor="black", alpha=0.7)
    ax_hist.set_xlabel("t_saturated (s)")
    ax_hist.set_ylabel("count")
    ax_hist.set_title(f"Histogram (σ={arr.std():.2f}s, p99-p1={np.percentile(arr,99)-np.percentile(arr,1):.1f}s)")
    ax_hist.grid(True, alpha=0.3)
    out = plots_dir / "ex_t_sat_cdf.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def buffer_at_sat_boxplot(per_commodity: list[dict], plots_dir: Path) -> Path:
    """Boxplot of max_enc per src DKMS."""
    by_src = defaultdict(list)
    for r in per_commodity:
        by_src[r["src"]].append(_safe_int(r.get("max_enc", "0")))
    srcs = sorted(by_src)
    data = [by_src[s] for s in srcs]
    fig, ax = plt.subplots(figsize=(14, 5))
    bp = ax.boxplot(data, labels=srcs, showmeans=True, patch_artist=True)
    for patch in bp["boxes"]:
        patch.set_facecolor("#a8d8ea")
    ax.set_xlabel("source DKMS")
    ax.set_ylabel("max_enc reached (keys in buffer)")
    ax.set_title("Per-DKMS distribution of max ENC buffer fill (over its 19 peers)")
    ax.tick_params(axis="x", labelrotation=90, labelsize=8)
    ax.grid(True, alpha=0.3, axis="y")
    out = plots_dir / "ex_buffer_at_sat.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def emit_rate_band(generator_state: list[dict], plots_dir: Path) -> Path:
    """Mean ± stdev of observed_keys_per_s across all commodities, over time."""
    by_t = defaultdict(list)
    for r in generator_state:
        t = _safe_float(r["t_seconds"])
        rate = _safe_float(r.get("observed_keys_per_s", "0"))
        # bucket by 1-second
        by_t[round(t)].append(rate)
    if not by_t:
        return plots_dir / "ex_emit_band.png"
    ts = sorted(by_t)
    means = [np.mean(by_t[t]) for t in ts]
    stds = [np.std(by_t[t]) for t in ts]
    p25 = [np.percentile(by_t[t], 25) for t in ts]
    p75 = [np.percentile(by_t[t], 75) for t in ts]
    fig, ax = plt.subplots(figsize=(14, 6))
    ax.plot(ts, means, lw=2, label="mean", color="C0")
    ax.fill_between(ts, np.array(means) - np.array(stds), np.array(means) + np.array(stds), alpha=0.25, color="C0", label="±1σ")
    ax.fill_between(ts, p25, p75, alpha=0.18, color="C1", label="p25–p75")
    ax.set_xlabel("seconds since first emit")
    ax.set_ylabel("observed_keys_per_s")
    ax.set_title("Aggregate emit rate over time — mean / ±σ / p25-p75")
    ax.grid(True, alpha=0.3)
    ax.legend()
    out = plots_dir / "ex_emit_band.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def ack_pending_timeline(generator_state: list[dict], plots_dir: Path) -> Path:
    """Heatmap commodity × time of ack_pending counts (capped)."""
    rows: dict[str, dict[int, int]] = defaultdict(dict)
    for r in generator_state:
        k = f"{r['src']}->{r['peer']}"
        t = round(_safe_float(r["t_seconds"]))
        rows[k][t] = _safe_int(r.get("ack_pending", "0"))
    if not rows:
        return plots_dir / "ex_ack_pending_timeline.png"
    commodities = sorted(rows.keys())
    all_t = sorted({t for d in rows.values() for t in d})
    if not all_t:
        return plots_dir / "ex_ack_pending_timeline.png"
    t_min, t_max = all_t[0], all_t[-1]
    ts = list(range(t_min, t_max + 1))
    M = np.zeros((len(commodities), len(ts)))
    for i, k in enumerate(commodities):
        d = rows[k]
        for j, t in enumerate(ts):
            M[i, j] = d.get(t, 0)
    fig, ax = plt.subplots(figsize=(14, max(6, len(commodities) * 0.025)))
    # cap at 99th percentile to keep heatmap readable
    cap = max(1, np.percentile(M, 99))
    im = ax.imshow(np.clip(M, 0, cap), aspect="auto", cmap="hot", origin="lower")
    ax.set_xlabel("seconds since first emit")
    ax.set_ylabel("commodity index")
    ax.set_title(f"ack_pending heatmap ({len(commodities)} commodities, cap={int(cap)} keys)")
    fig.colorbar(im, ax=ax, label="ack_pending (clipped)")
    out = plots_dir / "ex_ack_pending_timeline.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def per_src_sat_bar(per_commodity: list[dict], plots_dir: Path) -> Path:
    """Bar chart sat/total per src DKMS."""
    by_src = defaultdict(lambda: [0, 0])  # [sat, total]
    for r in per_commodity:
        by_src[r["src"]][1] += 1
        if r.get("saturated", "").lower() == "true":
            by_src[r["src"]][0] += 1
    srcs = sorted(by_src)
    sats = [by_src[s][0] for s in srcs]
    totals = [by_src[s][1] for s in srcs]
    fig, ax = plt.subplots(figsize=(14, 5))
    x = np.arange(len(srcs))
    ax.bar(x, totals, color="#e0e0e0", label="total peers")
    ax.bar(x, sats, color="#4caf50", label="saturated")
    ax.set_xticks(x)
    ax.set_xticklabels(srcs, rotation=90, fontsize=8)
    ax.set_ylabel("peer count")
    ax.set_title("Saturated peers per source DKMS")
    ax.legend()
    ax.grid(True, alpha=0.3, axis="y")
    for i, (s, t) in enumerate(zip(sats, totals)):
        ax.text(i, t + 0.2, f"{s}/{t}", ha="center", fontsize=7)
    out = plots_dir / "ex_per_src_sat.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def sdn_vs_observed_scatter(per_commodity: list[dict], plots_dir: Path) -> Path:
    """Scatter last_observed vs last_sdn_rate per commodity."""
    xs, ys = [], []
    for r in per_commodity:
        sdn = _safe_float(r.get("last_sdn_rate_keys_per_s", "0"))
        obs = _safe_float(r.get("last_observed_keys_per_s", "0"))
        xs.append(sdn)
        ys.append(obs)
    fig, ax = plt.subplots(figsize=(8, 8))
    ax.scatter(xs, ys, s=12, alpha=0.6)
    mx = max(max(xs, default=1), max(ys, default=1)) * 1.05
    ax.plot([0, mx], [0, mx], "r--", alpha=0.6, label="y=x (perfect tracking)")
    ax.set_xlabel("SDN-dictated rate (keys/s)")
    ax.set_ylabel("observed rate (keys/s)")
    ax.set_title(f"Observed vs SDN-dictated rate ({len(xs)} commodities)")
    ax.grid(True, alpha=0.3)
    ax.legend()
    out = plots_dir / "ex_sdn_vs_observed_scatter.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    return out


def _load_requests_csv(path: Path) -> list[dict[str, Any]]:
    if not path.exists():
        return []
    import csv
    rows: list[dict[str, Any]] = []
    with path.open() as f:
        reader = csv.DictReader(f)
        for r in reader:
            try:
                rows.append({
                    "t": _safe_float(r.get("emitted_at_epoch", "0")),
                    "lat": _safe_float(r.get("elapsed_seconds", "0")),
                    "status": _safe_int(r.get("status_code", "0")),
                    "src": r.get("sae_id", ""),
                    "dst": r.get("slave_sae_id", ""),
                })
            except Exception:
                continue
    return rows


def loadtest_plots(base_dir: Path, plots_dir: Path) -> list[Path]:
    """Generate extra loadtest plots from loadtest_requests.csv +
    loadtest_analysis.json."""
    data_dir = base_dir / "data"
    outs: list[Path] = []
    req_csv = next(iter(data_dir.glob("loadtest_requests*.csv")), None)
    rows = _load_requests_csv(req_csv) if req_csv else []
    if not rows:
        return outs
    # Normalize time to first request
    t0 = min(r["t"] for r in rows)
    for r in rows:
        r["t"] -= t0

    # 1) Requests per second over time, by status group
    bins = np.arange(0, max(r["t"] for r in rows) + 1, 1.0)
    groups = [
        ("2xx OK", [200, 201, 202, 204], "#2ecc71"),
        ("429 throttled", [429], "#e67e22"),
        ("5xx server", list(range(500, 600)), "#e74c3c"),
        ("4xx other", [c for c in range(400, 500) if c != 429], "#f1c40f"),
        ("conn err", [0], "#7f8c8d"),
    ]
    fig, ax = plt.subplots(figsize=(14, 6))
    for name, codes, color in groups:
        ts = [r["t"] for r in rows if r["status"] in codes]
        if not ts:
            continue
        hist, _ = np.histogram(ts, bins=bins)
        ax.plot(bins[:-1], hist, label=name, color=color, lw=1.5)
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("requests per second")
    ax.set_title(f"Requests/s by status over time ({len(rows)} total)")
    ax.legend()
    ax.grid(True, alpha=0.3)
    out = plots_dir / "ex_loadtest_rps_by_status.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    outs.append(out)

    # 2) Cumulative request counts by status
    fig, ax = plt.subplots(figsize=(14, 6))
    for name, codes, color in groups:
        ts = sorted([r["t"] for r in rows if r["status"] in codes])
        if not ts:
            continue
        y = np.arange(1, len(ts) + 1)
        ax.plot(ts, y, label=f"{name} ({len(ts)})", color=color, lw=1.8)
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("cumulative count")
    ax.set_title("Cumulative requests by status")
    ax.legend()
    ax.grid(True, alpha=0.3)
    out = plots_dir / "ex_loadtest_cumulative.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    outs.append(out)

    # 3) 429 fraction over time
    fig, ax = plt.subplots(figsize=(14, 5))
    window = 5.0  # seconds
    centers = np.arange(window / 2, max(r["t"] for r in rows), window / 2)
    frac_429 = []
    frac_ok = []
    for c in centers:
        in_win = [r for r in rows if abs(r["t"] - c) <= window / 2]
        if not in_win:
            frac_429.append(0)
            frac_ok.append(0)
            continue
        n429 = sum(1 for r in in_win if r["status"] == 429)
        nok = sum(1 for r in in_win if 200 <= r["status"] < 300)
        frac_429.append(100.0 * n429 / len(in_win))
        frac_ok.append(100.0 * nok / len(in_win))
    ax.fill_between(centers, frac_ok, color="#2ecc71", alpha=0.7, label="% 2xx OK")
    ax.fill_between(centers, frac_ok, np.array(frac_ok) + np.array(frac_429),
                    color="#e67e22", alpha=0.7, label="% 429")
    ax.set_xlabel(f"seconds since first request (sliding window {window}s)")
    ax.set_ylabel("% requests in window")
    ax.set_ylim(0, 100)
    ax.set_title("Throttling timeline — fraction 429 vs 2xx (sliding window)")
    ax.legend(loc="lower right")
    ax.grid(True, alpha=0.3)
    out = plots_dir / "ex_loadtest_throttling.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    outs.append(out)

    # 4) Latency CDF per status class
    fig, ax = plt.subplots(figsize=(10, 6))
    for name, codes, color in groups:
        lats = sorted([r["lat"] for r in rows if r["status"] in codes and r["lat"] > 0])
        if not lats:
            continue
        arr = np.array(lats)
        y = np.arange(1, len(arr) + 1) / len(arr)
        ax.plot(arr, y, label=f"{name} (n={len(arr)})", color=color, lw=1.5)
    ax.set_xlabel("request latency (s)")
    ax.set_ylabel("CDF")
    ax.set_title("Latency CDF per status class")
    ax.set_xscale("log")
    ax.grid(True, alpha=0.3, which="both")
    ax.legend()
    out = plots_dir / "ex_loadtest_latency_per_class.png"
    fig.tight_layout()
    fig.savefig(out, dpi=110)
    plt.close(fig)
    outs.append(out)

    # 5) Per-SAE-pair status summary (top 20 most active pairs)
    from collections import Counter
    pair_count = Counter()
    pair_429 = Counter()
    for r in rows:
        key = f"{r['src']}→{r['dst']}"
        pair_count[key] += 1
        if r["status"] == 429:
            pair_429[key] += 1
    top = pair_count.most_common(20)
    if top:
        keys = [k for k, _ in top]
        totals = [v for _, v in top]
        rates_429 = [pair_429[k] for k in keys]
        fig, ax = plt.subplots(figsize=(14, 7))
        x = np.arange(len(keys))
        ax.bar(x, totals, color="#e0e0e0", label="total requests")
        ax.bar(x, rates_429, color="#e67e22", label="429s")
        ax.set_xticks(x)
        ax.set_xticklabels(keys, rotation=70, fontsize=7)
        ax.set_ylabel("request count")
        ax.set_title("Top 20 SAE pairs by request volume — total vs 429")
        ax.legend()
        ax.grid(True, alpha=0.3, axis="y")
        out = plots_dir / "ex_loadtest_top_pairs.png"
        fig.tight_layout()
        fig.savefig(out, dpi=110)
        plt.close(fig)
        outs.append(out)

    return outs


def main(output_dir: str) -> dict[str, Any]:
    base = Path(output_dir)
    data = base / "data"
    plots = base / "plots"
    plots.mkdir(exist_ok=True)

    gs = _read_csv(data / "generator_state.csv")
    pc = _read_csv(data / "per_commodity.csv")
    produced: list[Path] = []
    if pc:
        produced.append(heatmap_rate(pc, plots))
        produced.append(t_sat_cdf(pc, plots))
        produced.append(buffer_at_sat_boxplot(pc, plots))
        produced.append(per_src_sat_bar(pc, plots))
        produced.append(sdn_vs_observed_scatter(pc, plots))
    if gs:
        produced.append(emit_rate_band(gs, plots))
        produced.append(ack_pending_timeline(gs, plots))
    produced.extend(loadtest_plots(base, plots))
    return {"plots": [str(p) for p in produced]}


if __name__ == "__main__":
    import sys
    out = main(sys.argv[1] if len(sys.argv) > 1 else "tests/results/eks-mcmcf-er20")
    for p in out["plots"]:
        print(p)
