"""Genera gráficas del etsi014 roundtrip loadtest.

Lee requests.csv (columnas: t_emit, pair_id, master_sae, slave_sae,
master_host_id, slave_host_id, key_id, enc_ms, dec_ms, ok_enc, ok_dec,
match, err) y produce:

  - rps_over_time.png       enc+dec request rate (round-trips/s)
  - latency_split_time.png  enc p50/p95 y dec p50/p95 a lo largo del tiempo
  - latency_hist.png        histogramas enc y dec lado a lado
  - match_over_time.png     % round-trips con match=1 por bin de 5s
  - keys_per_dkms.png       round-trips agregados por DKMS master

Uso:
    python3 plot_roundtrip.py <run_dir>
        run_dir debe contener requests.csv y summary.json
"""
from __future__ import annotations

import argparse
import csv
import collections
import json
import pathlib

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def load(run_dir: pathlib.Path):
    rows = list(csv.DictReader((run_dir / "requests.csv").open()))
    summary = json.loads((run_dir / "summary.json").read_text())
    return rows, summary


def main(run_dir: pathlib.Path) -> None:
    rows, summary = load(run_dir)
    plots = run_dir / "plots"
    plots.mkdir(parents=True, exist_ok=True)

    n = len(rows)
    print(f"loaded {n} round-trips")
    if n == 0:
        print("no rows — nothing to plot")
        return

    t = np.array([float(r["t_emit"]) for r in rows])
    enc = np.array([float(r["enc_ms"]) for r in rows])
    dec = np.array([float(r["dec_ms"]) for r in rows])
    ok_enc = np.array([r["ok_enc"] == "1" for r in rows])
    ok_dec = np.array([r["ok_dec"] == "1" for r in rows])
    match = np.array([r["match"] == "1" for r in rows])
    master_host = np.array([int(r["master_host_id"]) for r in rows])

    t0 = t.min()
    t_rel = t - t0
    duration = float(t_rel.max()) + 1

    # 1. RPS over time (1s bins).
    bins = np.arange(0, duration + 1, 1.0)
    hist_all, _ = np.histogram(t_rel, bins=bins)
    hist_match, _ = np.histogram(t_rel[match], bins=bins)
    centers = (bins[:-1] + bins[1:]) / 2

    fig, ax = plt.subplots(figsize=(12, 5))
    ax.plot(centers, hist_all, label="all round-trips", color="steelblue", lw=1.5)
    ax.plot(centers, hist_match, label="match (key bytes equal)", color="seagreen", lw=1.5, ls="--")
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("round-trips/s")
    sim_id = summary.get('sim_id', 'N/A')
    ramp_s = summary.get('ramp_start', '?'); ramp_e = summary.get('ramp_end', '?')
    lam = summary.get('lambda_rps', '?'); n_workers = summary.get('n_workers')
    extra = f", {n_workers} workers" if n_workers else ""
    title = f"ETSI 014 round-trip RPS — sim {sim_id}, ramp {ramp_s}→{ramp_e}, λ={lam}r/s{extra}"
    ax.set_title(title)
    ax.grid(True, alpha=0.3)
    ax.legend()
    fig.tight_layout()
    fig.savefig(plots / "rps_over_time.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'rps_over_time.png'}")

    # 2. Latency split (enc / dec) p50 + p95.
    window = 5.0
    win_t, enc_p50, enc_p95, dec_p50, dec_p95 = [], [], [], [], []
    for start in np.arange(0, duration, window):
        sel = (t_rel >= start) & (t_rel < start + window)
        if sel.sum() < 5:
            continue
        win_t.append(start + window / 2)
        enc_w = sorted(enc[sel & ok_enc])
        dec_w = sorted(dec[sel & ok_dec])
        if enc_w:
            enc_p50.append(enc_w[len(enc_w) // 2])
            enc_p95.append(enc_w[int(len(enc_w) * 0.95)])
        else:
            enc_p50.append(0); enc_p95.append(0)
        if dec_w:
            dec_p50.append(dec_w[len(dec_w) // 2])
            dec_p95.append(dec_w[int(len(dec_w) * 0.95)])
        else:
            dec_p50.append(0); dec_p95.append(0)

    fig, ax = plt.subplots(figsize=(12, 5))
    ax.plot(win_t, enc_p50, label="enc p50", color="steelblue", lw=1.7)
    ax.plot(win_t, enc_p95, label="enc p95", color="steelblue", lw=1.0, alpha=0.7, ls="--")
    ax.plot(win_t, dec_p50, label="dec p50", color="orange", lw=1.7)
    ax.plot(win_t, dec_p95, label="dec p95", color="orange", lw=1.0, alpha=0.7, ls="--")
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("latency (ms)")
    ax.set_title("ETSI 014 enc vs dec latency (5s window, p50 solid / p95 dashed)")
    ax.grid(True, alpha=0.3)
    ax.legend()
    fig.tight_layout()
    fig.savefig(plots / "latency_split_time.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'latency_split_time.png'}")

    # 3. Hist enc + dec side-by-side.
    fig, (ax1, ax2) = plt.subplots(1, 2, figsize=(14, 5), sharey=True)
    enc_ok = enc[ok_enc]
    dec_ok = dec[ok_dec]
    for ax, vals, name, color in (
        (ax1, enc_ok, "enc_keys (master)", "steelblue"),
        (ax2, dec_ok, "dec_keys (slave)", "orange"),
    ):
        if len(vals):
            ax.hist(vals, bins=60, color=color, edgecolor="white")
            p50, p95, p99 = np.percentile(vals, [50, 95, 99])
            ax.axvline(p50, color="black", ls="--", label=f"p50={p50:.0f}ms")
            ax.axvline(p95, color="firebrick", ls="--", label=f"p95={p95:.0f}ms")
            ax.axvline(p99, color="purple", ls=":", label=f"p99={p99:.0f}ms")
        ax.set_xlabel("latency (ms)")
        ax.set_title(f"{name}  (n={len(vals)})")
        ax.grid(True, alpha=0.3)
        ax.legend()
    ax1.set_ylabel("count")
    fig.suptitle("ETSI 014 round-trip latency histograms")
    fig.tight_layout()
    fig.savefig(plots / "latency_hist.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'latency_hist.png'}")

    # 4. Match percentage over time.
    fig, ax = plt.subplots(figsize=(12, 4))
    win = 5.0
    bin_t, bin_match = [], []
    for start in np.arange(0, duration, win):
        sel = (t_rel >= start) & (t_rel < start + win)
        if sel.sum() < 3:
            continue
        bin_t.append(start + win / 2)
        bin_match.append(100.0 * match[sel].mean())
    ax.plot(bin_t, bin_match, color="seagreen", lw=1.7)
    ax.set_ylim(0, 102)
    ax.set_xlabel("seconds since first request")
    ax.set_ylabel("match %")
    ax.set_title(f"ETSI 014 byte-equality match % over time (overall={summary.get('match_pct','?')}%)")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(plots / "match_over_time.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'match_over_time.png'}")

    # 5. Round-trips per master DKMS.
    counts = collections.Counter(int(x) for x in master_host)
    labels = sorted(counts)
    values = [counts[k] for k in labels]
    fig, ax = plt.subplots(figsize=(10, 5))
    ax.bar([str(x) for x in labels], values, color="steelblue", edgecolor="white")
    ax.set_xlabel("master DKMS host_id")
    ax.set_ylabel("round-trips count")
    ax.set_title(f"Round-trips per master DKMS (total={n})")
    ax.grid(True, alpha=0.3, axis="y")
    fig.tight_layout()
    fig.savefig(plots / "keys_per_dkms.png", dpi=130)
    plt.close(fig)
    print(f"wrote {plots / 'keys_per_dkms.png'}")

    print("\ndone")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    args = p.parse_args()
    main(pathlib.Path(args.run_dir))
