#!/usr/bin/env python3
"""Render PNG plots for a run from the CSVs produced by analyze_run.py.

  python3 plot_run.py --csv-dir <dir> --raw-load <load.csv> --out <plots dir> --name <run>

Produces (whatever the inputs allow):
  * buffer_fill.png      — total buffered OTP keys vs time
  * load_throughput.png  — ok/s and 429/s vs time (the SAE ramp / saturation)
  * latency.png          — latency p50/p99 vs time + histogram of successful reqs

Uses the Agg backend (no display). Needs matplotlib (present in the system
python3 on CESGA: `python3 plot_run.py ...`, NOT the python module).
"""
from __future__ import annotations

import argparse
import csv
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402


def read_csv(path: Path):
    if not path.exists():
        return []
    with open(path) as f:
        return list(csv.DictReader(f))


def plot_buffer(rows, out: Path, name: str) -> bool:
    if not rows:
        return False
    t = [float(r["t_s"]) for r in rows]
    y = [float(r["total_enc_keys"]) / 1e6 for r in rows]
    fig, ax = plt.subplots(figsize=(8, 4))
    ax.plot(t, y, "-o", ms=3, color="#1f77b4")
    ax.fill_between(t, y, alpha=0.15, color="#1f77b4")
    ax.set(xlabel="time (s)", ylabel="buffered OTP keys (millions)",
           title=f"{name} — buffer fill (Σ enc over all DKMS↔peer buffers)")
    ax.grid(True, alpha=0.3)
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig)
    return True


def plot_throughput(rows, out: Path, name: str) -> bool:
    if not rows:
        return False
    t = [float(r["t_s"]) for r in rows]
    ok = [float(r["ok_per_s"]) for r in rows]
    r4 = [float(r["r429_per_s"]) for r in rows]
    fig, ax = plt.subplots(figsize=(8, 4))
    ax.plot(t, ok, "-o", ms=3, color="#2ca02c", label="200 OK /s")
    ax.plot(t, r4, "-s", ms=3, color="#d62728", label="429 /s (backpressure)")
    ax.set(xlabel="time (s)", ylabel="requests / s",
           title=f"{name} — SAE enc_keys throughput")
    ax.legend(); ax.grid(True, alpha=0.3)
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig)
    return True


def plot_latency(ts_rows, raw_rows, out: Path, name: str) -> bool:
    lat = [float(r["latency_ms"]) for r in raw_rows if r.get("status") == "200"]
    if not ts_rows and not lat:
        return False
    fig, axes = plt.subplots(1, 2, figsize=(11, 4))
    if ts_rows:
        t = [float(r["t_s"]) for r in ts_rows]
        axes[0].plot(t, [float(r["p50_ms"]) for r in ts_rows], "-o", ms=3, label="p50")
        axes[0].plot(t, [float(r["p99_ms"]) for r in ts_rows], "-s", ms=3, label="p99")
        axes[0].set(xlabel="time (s)", ylabel="latency (ms)", title=f"{name} — latency over time")
        axes[0].legend(); axes[0].grid(True, alpha=0.3)
    if lat:
        clip = sorted(lat)[int(0.99 * len(lat))] if len(lat) > 1 else max(lat)
        axes[1].hist([x for x in lat if x <= max(clip, 1.0)], bins=40, color="#9467bd")
        axes[1].set(xlabel="latency (ms)", ylabel="count (200 OK)",
                    title=f"{name} — successful-request latency (≤p99)")
        axes[1].grid(True, alpha=0.3)
    fig.tight_layout(); fig.savefig(out, dpi=110); plt.close(fig)
    return True


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--csv-dir", required=True)
    ap.add_argument("--raw-load", default=None, help="path to load.csv (for latency hist)")
    ap.add_argument("--out", required=True)
    ap.add_argument("--name", default="run")
    args = ap.parse_args()
    cdir, out = Path(args.csv_dir), Path(args.out)
    out.mkdir(parents=True, exist_ok=True)

    made = []
    if plot_buffer(read_csv(cdir / "buffer_fill.csv"), out / "buffer_fill.png", args.name):
        made.append("buffer_fill.png")
    ts = read_csv(cdir / "load_timeseries.csv")
    if plot_throughput(ts, out / "load_throughput.png", args.name):
        made.append("load_throughput.png")
    raw = read_csv(Path(args.raw_load)) if args.raw_load else []
    if plot_latency(ts, raw, out / "latency.png", args.name):
        made.append("latency.png")
    print(f"[plot] {args.name}: {made or 'nothing to plot'} → {out}")


if __name__ == "__main__":
    main()
