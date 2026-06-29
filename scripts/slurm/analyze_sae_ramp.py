#!/usr/bin/env python3
"""SAE-indexed analysis of an ETSI-014 round-trip ramp (roundtrip.py output).

The stock plots (plot_match_vs_429.py, plot_roundtrip.py) are TIME-indexed.
The question "how many SAEs does it support before saturating?" needs the curve
indexed by the number of *active SAEs*, not by wall-clock seconds.

This reconstructs the active-SAE level EMPIRICALLY (robust to warmup/jitter
offsets and per-worker ramp skew): each (worker_id, pair_id) pair contributes
2 SAEs (master+slave) and is considered "active" from the instant it emits its
first request. The ramp only ADDS pairs (never removes until teardown), so

    active_saes(t) = 2 * #{pairs whose first request t_emit <= t}

is monotone and recovers the true ramp without trusting the nominal schedule.

Each request is then assigned the active-SAE level at its emit time (rounded to
the nominal 100-SAE grid), grouped, and per-level we compute: round-trip match
rate, 429 backpressure rate, enc/dec ok rates, an error breakdown, round-trip
latency percentiles and throughput. Finally it locates the saturation knee
(highest SAE level still cleanly served) and writes a SAE-indexed plot + CSVs.

Reads  : <rt-dir>/worker-*/requests.csv  (13-col canonical schema)
Writes : <rt-dir>/sae_ramp_by_level.csv
         <rt-dir>/active_sae_timeseries.csv
         <rt-dir>/sae_ramp.png
Stdlib + matplotlib (Agg). Usage: analyze_sae_ramp.py <rt-dir> [--grid 100] [--match-thresh 0.995] [--429-thresh 0.005]
"""
from __future__ import annotations

import argparse
import bisect
import csv
import glob
import os
import re
from collections import defaultdict


def pct(sorted_vals, p):
    if not sorted_vals:
        return float("nan")
    k = (len(sorted_vals) - 1) * (p / 100.0)
    lo = int(k)
    hi = min(lo + 1, len(sorted_vals) - 1)
    return sorted_vals[lo] + (sorted_vals[hi] - sorted_vals[lo]) * (k - lo)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("rt_dir", help="run dir containing worker-*/requests.csv")
    ap.add_argument("--grid", type=int, default=100, help="SAE-level rounding grid")
    ap.add_argument("--match-thresh", type=float, default=0.995,
                    help="min round-trip match rate to count a level as 'cleanly served'")
    ap.add_argument("--http429-thresh", type=float, default=0.005,
                    help="max 429 fraction to count a level as 'cleanly served'")
    args = ap.parse_args()

    files = sorted(glob.glob(os.path.join(args.rt_dir, "worker-*", "requests.csv")))
    if not files:
        raise SystemExit(f"no worker-*/requests.csv under {args.rt_dir}")

    # rows: (t_emit, gkey, ok_enc, ok_dec, match, enc_ms, dec_ms, err)
    rows = []
    first_emit: dict[tuple, float] = {}
    wid_re = re.compile(r"worker-(\d+)")
    for f in files:
        wid = int(wid_re.search(f).group(1))
        with open(f, newline="") as fh:
            r = csv.DictReader(fh)
            for d in r:
                try:
                    t = float(d["t_emit"])
                except (ValueError, KeyError):
                    continue
                gkey = (wid, d["pair_id"])
                if gkey not in first_emit or t < first_emit[gkey]:
                    first_emit[gkey] = t
                rows.append((
                    t, gkey,
                    int(d["ok_enc"] or 0), int(d["ok_dec"] or 0), int(d["match"] or 0),
                    float(d["enc_ms"] or 0.0), float(d["dec_ms"] or 0.0), d["err"] or "",
                ))

    if not rows:
        raise SystemExit("no rows parsed")

    rows.sort(key=lambda x: x[0])
    t0 = rows[0][0]
    n_pairs_total = len(first_emit)

    # monotone activation timeline: sorted first-emit times -> active pairs via bisect
    activations = sorted(first_emit.values())

    def active_saes_at(t: float) -> int:
        return 2 * bisect.bisect_right(activations, t)

    # active-SAE timeseries (1 s buckets), for an audit/sanity plot
    ts_rows = []
    t_end = rows[-1][0]
    tb = t0
    while tb <= t_end + 1e-9:
        ts_rows.append((round(tb - t0, 1), active_saes_at(tb)))
        tb += 1.0
    with open(os.path.join(args.rt_dir, "active_sae_timeseries.csv"), "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["t_rel_s", "active_saes"])
        w.writerows(ts_rows)

    # per-level aggregation (level = active SAEs rounded to grid)
    G = args.grid
    agg = defaultdict(lambda: {
        "n": 0, "match": 0, "ok_enc": 0, "ok_dec": 0,
        "enc_429": 0, "dec_429": 0, "enc_err": 0, "dec_err": 0, "exc": 0,
        "rt_ms": [], "t_lo": float("inf"), "t_hi": float("-inf"),
    })
    HTTP429 = re.compile(r"HTTP 429")
    ENC429 = re.compile(r"^enc HTTP 429")
    DEC429 = re.compile(r"^dec HTTP 429")

    for (t, _g, ok_enc, ok_dec, match, enc_ms, dec_ms, err) in rows:
        lvl = active_saes_at(t)
        lvl = int(round(lvl / G) * G)
        lvl = max(G, min(lvl, 2 * n_pairs_total))
        a = agg[lvl]
        a["n"] += 1
        a["match"] += match
        a["ok_enc"] += ok_enc
        a["ok_dec"] += ok_dec
        a["t_lo"] = min(a["t_lo"], t)
        a["t_hi"] = max(a["t_hi"], t)
        if match:
            a["rt_ms"].append(enc_ms + dec_ms)
        if err:
            if ENC429.search(err):
                a["enc_429"] += 1
            elif DEC429.search(err):
                a["dec_429"] += 1
            elif "exc" in err:
                a["exc"] += 1
            elif err.startswith("enc"):
                a["enc_err"] += 1
            else:
                a["dec_err"] += 1

    levels = sorted(agg)
    out_csv = os.path.join(args.rt_dir, "sae_ramp_by_level.csv")
    cols = ["sae_level", "n_req", "match_pct", "http429_pct", "ok_enc_pct", "ok_dec_pct",
            "enc_429", "dec_429", "enc_err", "dec_err", "exc",
            "rt_p50_ms", "rt_p90_ms", "rt_p99_ms", "thru_rps"]
    table = []
    for lvl in levels:
        a = agg[lvl]
        n = a["n"]
        dur = max(1e-6, a["t_hi"] - a["t_lo"])
        s = sorted(a["rt_ms"])
        h429 = a["enc_429"] + a["dec_429"]
        table.append({
            "sae_level": lvl,
            "n_req": n,
            "match_pct": 100.0 * a["match"] / n,
            "http429_pct": 100.0 * h429 / n,
            "ok_enc_pct": 100.0 * a["ok_enc"] / n,
            "ok_dec_pct": 100.0 * a["ok_dec"] / n,
            "enc_429": a["enc_429"], "dec_429": a["dec_429"],
            "enc_err": a["enc_err"], "dec_err": a["dec_err"], "exc": a["exc"],
            "rt_p50_ms": pct(s, 50), "rt_p90_ms": pct(s, 90), "rt_p99_ms": pct(s, 99),
            "thru_rps": n / dur,
        })
    with open(out_csv, "w", newline="") as fh:
        w = csv.DictWriter(fh, fieldnames=cols)
        w.writeheader()
        for r in table:
            w.writerow({k: (round(v, 3) if isinstance(v, float) else v) for k, v in r.items()})

    # saturation knee: highest level for which all levels up to it are cleanly served
    mt, h429t = args.match_thresh * 100.0, args.http429_thresh * 100.0
    supported = 0
    first_bad = None
    for r in table:
        clean = (r["match_pct"] >= mt) and (r["http429_pct"] <= h429t)
        if clean and first_bad is None:
            supported = r["sae_level"]
        elif not clean and first_bad is None:
            first_bad = r

    print(f"=== SAE ramp analysis ({args.rt_dir}) ===")
    print(f"pairs={n_pairs_total}  max SAEs={2*n_pairs_total}  requests={len(rows)}  "
          f"levels={len(levels)} (grid {G})")
    print(f"clean-service criteria: match% >= {mt:.2f} AND 429% <= {h429t:.3f}")
    print(f">>> SAEs cleanly supported (contiguous from start): {supported}")
    if first_bad:
        print(f">>> first level to break it: {first_bad['sae_level']} SAEs "
              f"(match={first_bad['match_pct']:.2f}%  429={first_bad['http429_pct']:.2f}%)")
    print()
    print(f"{'SAEs':>5} {'n':>7} {'match%':>7} {'429%':>6} {'okEnc%':>7} {'okDec%':>7} "
          f"{'p50ms':>7} {'p99ms':>8} {'rps':>7}")
    for r in table:
        print(f"{r['sae_level']:>5} {r['n_req']:>7} {r['match_pct']:>7.2f} {r['http429_pct']:>6.2f} "
              f"{r['ok_enc_pct']:>7.2f} {r['ok_dec_pct']:>7.2f} "
              f"{r['rt_p50_ms']:>7.1f} {r['rt_p99_ms']:>8.1f} {r['thru_rps']:>7.1f}")

    # ── plot: SAE-indexed match%/429% (+ latency) ──
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except Exception as e:  # noqa: BLE001
        print(f"[plot skipped: {e}]")
        return

    xs = [r["sae_level"] for r in table]
    fig, (ax1, ax3) = plt.subplots(2, 1, figsize=(10, 8), sharex=True,
                                   gridspec_kw={"height_ratios": [2, 1]})
    ax1.plot(xs, [r["match_pct"] for r in table], "-o", color="#2ca02c", label="round-trip match %", ms=4)
    ax1.set_ylabel("match %", color="#2ca02c")
    ax1.tick_params(axis="y", labelcolor="#2ca02c")
    ax1.set_ylim(-2, 102)
    ax1.axhline(mt, ls="--", lw=0.8, color="#2ca02c", alpha=0.5)
    ax2 = ax1.twinx()
    ax2.plot(xs, [r["http429_pct"] for r in table], "-s", color="#d62728", label="429 backpressure %", ms=4)
    ax2.set_ylabel("429 %", color="#d62728")
    ax2.tick_params(axis="y", labelcolor="#d62728")
    if supported:
        ax1.axvline(supported, ls=":", color="k", alpha=0.6)
        ax1.annotate(f"clean ≤ {supported} SAEs", (supported, 50),
                     xytext=(6, 0), textcoords="offset points", fontsize=9, rotation=90, va="center")
    ax1.set_title(f"ETSI-014 round-trip SAE ramp — BA N=20 (m=2) — supported ≈ {supported} SAEs")
    l1, lab1 = ax1.get_legend_handles_labels()
    l2, lab2 = ax2.get_legend_handles_labels()
    ax1.legend(l1 + l2, lab1 + lab2, loc="center left", fontsize=9)

    ax3.plot(xs, [r["rt_p50_ms"] for r in table], "-o", color="#1f77b4", label="p50", ms=3)
    ax3.plot(xs, [r["rt_p99_ms"] for r in table], "-^", color="#ff7f0e", label="p99", ms=3)
    ax3.set_ylabel("round-trip latency (ms)")
    ax3.set_xlabel("active SAEs")
    ax3.legend(fontsize=9)
    ax3.grid(alpha=0.3)
    fig.tight_layout()
    out_png = os.path.join(args.rt_dir, "sae_ramp.png")
    fig.savefig(out_png, dpi=110)
    print(f"\nwrote {out_csv}\nwrote {out_png}")


if __name__ == "__main__":
    main()
