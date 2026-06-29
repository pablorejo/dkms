#!/usr/bin/env python3
"""Analyze a finished deployment run → text tables + analyzable CSVs.

Reads, from a run dir:
  * logs/dkms-*.log  → `generator.state` lines (enc buffer level per peer, every ~5s)
    → aggregate buffered-key curve over time  → buffer_fill.csv
  * logs/load.csv    → per-request (epoch,sae,status,latency_ms) from load.py
    → per-bucket 200/s, 429/s, latency p50/p99            → load_timeseries.csv

Prints the tables (for ANALYSIS.md); with --csv-dir also writes the two CSVs
there. Stdlib only. Plot them with plot_run.py.
"""
from __future__ import annotations

import argparse
import re
from collections import defaultdict
from pathlib import Path

ANSI = re.compile(r"\x1b\[[0-9;]*m")
TS = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2})")
ENC = re.compile(r"\benc=(\d+)")
PEER = re.compile(r"\bpeer=(\S+)")
BARS = " ▁▂▃▄▅▆▇█"


def spark(vals):
    if not vals:
        return ""
    lo, hi = min(vals), max(vals)
    rng = (hi - lo) or 1.0
    return "".join(BARS[min(8, int((v - lo) / rng * 8))] for v in vals)


def hhmmss_to_s(ts: str) -> int:
    return int(ts[11:13]) * 3600 + int(ts[14:16]) * 60 + int(ts[17:19])


def buffer_series(run: Path):
    """→ list of (t_s, total_enc_keys, active_pairs)."""
    per_t: dict[int, dict[tuple[str, str], int]] = defaultdict(dict)
    for lf in sorted((run / "logs").glob("dkms-*.log")):
        dkms = lf.stem
        for line in lf.read_text(errors="replace").splitlines():
            if "generator.state" not in line:
                continue
            line = ANSI.sub("", line)
            mt, me, mp = TS.search(line), ENC.search(line), PEER.search(line)
            if mt and me and mp:
                per_t[hhmmss_to_s(mt.group(1))][(dkms, mp.group(1))] = int(me.group(1))
    if not per_t:
        return []
    t0 = min(per_t)
    last: dict[tuple[str, str], int] = {}
    rows = []
    for t in sorted(per_t):
        last.update(per_t[t])
        rows.append((t - t0, sum(last.values()), len(last)))
    return rows


def load_series(run: Path, bucket: int):
    """→ list of (t_s, ok_s, r429_s, err_s, p50_ms, p99_ms), plus totals dict."""
    csv = run / "logs" / "load.csv"
    if not csv.exists():
        return [], {}
    buckets: dict[int, dict[str, int]] = defaultdict(lambda: defaultdict(int))
    lat: dict[int, list[float]] = defaultdict(list)
    tot: dict[str, int] = defaultdict(int)
    t0 = None
    for r in csv.read_text().splitlines()[1:]:
        p = r.split(",")
        if len(p) != 4:
            continue
        ep, _sae, st, ms = float(p[0]), p[1], p[2], p[3]
        t0 = ep if t0 is None else min(t0, ep)
        b = int((ep - t0) // bucket)
        buckets[b][st] += 1
        tot[st] += 1
        if st == "200":
            lat[b].append(float(ms))
    rows = []
    for b in sorted(buckets):
        d = buckets[b]
        ls = sorted(lat[b])
        p50 = ls[len(ls) // 2] if ls else 0.0
        p99 = ls[min(len(ls) - 1, int(0.99 * len(ls)))] if ls else 0.0
        rows.append((b * bucket, d.get("200", 0) / bucket, d.get("429", 0) / bucket,
                     d.get("ERR", 0) / bucket, p50, p99))
    return rows, dict(tot)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("run")
    ap.add_argument("--bucket", type=int, default=2)
    ap.add_argument("--csv-dir", default=None, help="also write buffer_fill.csv + load_timeseries.csv here")
    args = ap.parse_args()
    run = Path(args.run)
    csv_dir = Path(args.csv_dir) if args.csv_dir else None
    if csv_dir:
        csv_dir.mkdir(parents=True, exist_ok=True)

    print(f"=== {run.name}: buffer-fill curve (generator.state) ===")
    bs = buffer_series(run)
    if bs:
        print(f"  {'t(s)':>5} {'total_enc_keys':>15} {'active(dkms,peer)':>18}")
        for dt, total, n in bs:
            print(f"  {dt:>5} {total:>15,} {n:>18}")
        print(f"  fill: {spark([r[1] for r in bs])}  (0 → {bs[-1][1]:,} keys)")
        if csv_dir:
            (csv_dir / "buffer_fill.csv").write_text(
                "t_s,total_enc_keys,active_pairs\n" +
                "".join(f"{t},{v},{n}\n" for t, v, n in bs))
    else:
        print("  (no generator.state samples)")

    print(f"\n=== {run.name}: SAE load time-series ({args.bucket}s buckets) ===")
    ls, tot = load_series(run, args.bucket)
    if ls:
        print(f"  {'t(s)':>5} {'ok/s':>7} {'429/s':>7} {'err/s':>7} {'p50ms':>6} {'p99ms':>6}")
        for t, ok, r4, er, p50, p99 in ls:
            print(f"  {t:>5} {ok:>7.0f} {r4:>7.0f} {er:>7.0f} {p50:>6.1f} {p99:>6.1f}")
        print(f"  ok/s: {spark([r[1] for r in ls])}  (peak {max(r[1] for r in ls):.0f}/s)")
        n = sum(tot.values())
        okp = 100.0 * tot.get('200', 0) / n if n else 0
        print(f"  totals: {tot}  ({okp:.0f}% ok)")
        if csv_dir:
            (csv_dir / "load_timeseries.csv").write_text(
                "t_s,ok_per_s,r429_per_s,err_per_s,p50_ms,p99_ms\n" +
                "".join(f"{t},{ok:.1f},{r4:.1f},{er:.1f},{p50:.2f},{p99:.2f}\n"
                        for t, ok, r4, er, p50, p99 in ls))
    else:
        print("  (no load.csv — smoke-only run)")


if __name__ == "__main__":
    main()
