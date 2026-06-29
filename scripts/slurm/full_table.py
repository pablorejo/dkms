#!/usr/bin/env python3
"""Aggregate the full-* campaign cells into analysis-ready CSVs + a matrix.

Auto-discovers tests/results/full-<topo>-n<N>/ cells (like summarize.py) and emits
to tests/results/full-n10-100/:
  cells.csv        — 1 row per cell: sizes, smoke, success/error rates, latencies,
                     SDN solve times, buffer fill. The per-cell master table.
  ramp_levels.csv  — long format, 1 row per (topo,N,sae_level): match%, 429%, thru,
                     latencies. The data behind the sae_ramp / match_vs_429 plots.
  timeseries.csv   — long format, 1 row per (topo,N,t_s): buffered keys + active
                     pairs over time (the data behind buffer_fill plots).
  MATRIX.md        — 5×10 grid (match% / 429% / STALL) for a quick read.

Reuses analyze_sae_ramp.py for the per-level breakdown (extracts the merged
requests.csv from each cell's roundtrip.tar.gz into a temp dir and runs it), so
the numbers match the plots exactly. Defensive: skips any missing piece.
"""
import csv
import json
import re
import statistics
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
RESULTS = REPO / "tests/results"
OUT = RESULTS / "full-n10-100"
ANSI = re.compile(r"\x1b\[[0-9;]*m")

TOPOS = ["ba", "er", "ba2", "rgg", "secoqc"]
NS = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100]


def solve_times(cell: Path):
    """Median/max/count of SDN MCMCF-λ solve times (s) from logs.tar.gz."""
    tgz = cell / "logs.tar.gz"
    if not tgz.exists():
        return (0, None, None)
    try:
        with tarfile.open(tgz) as t:
            m = next((n for n in t.getnames() if n.endswith("sdn.log")), None)
            if not m:
                return (0, None, None)
            txt = ANSI.sub("", t.extractfile(m).read().decode("utf-8", "replace"))
    except Exception:
        return (0, None, None)
    ms = [int(x) for x in re.findall(r"elapsed_ms=(\d+)", txt)]
    if not ms:
        return (0, None, None)
    return (len(ms), statistics.median(ms) / 1000.0, max(ms) / 1000.0)


def buffer_stats(cell: Path):
    """Linear fill rate (keys/s) and peak (M keys) from buffer_fill.csv."""
    f = cell / "buffer_fill.csv"
    if not f.exists():
        return (None, None)
    ts, ys = [], []
    with open(f) as fh:
        for row in csv.DictReader(fh):
            try:
                ts.append(float(row["t_s"])); ys.append(float(row["total_enc_keys"]))
            except (KeyError, ValueError):
                pass
    if len(ys) < 2:
        return (None, None)
    rng = ts[-1] - ts[0]
    rate = (max(ys) - ys[0]) / rng if rng > 0 else None
    return (rate, max(ys) / 1e6)


def ramp_levels(cell: Path, topo, n):
    """Per-SAE-level rows via analyze_sae_ramp.py on the cell's merged requests.csv."""
    tgz = cell / "roundtrip.tar.gz"
    if not tgz.exists():
        return []
    rows = []
    with tempfile.TemporaryDirectory() as td:
        wdir = Path(td) / "worker-0"
        wdir.mkdir(parents=True)
        try:
            with tarfile.open(tgz) as t:
                m = next((x for x in t.getnames() if x.endswith("requests.csv")), None)
                if not m:
                    return []
                (wdir / "requests.csv").write_bytes(t.extractfile(m).read())
        except Exception:
            return []
        try:
            subprocess.run([sys.executable, str(REPO / "scripts/slurm/analyze_sae_ramp.py"), td],
                           check=True, capture_output=True, timeout=600)
        except Exception:
            return []
        lvl_csv = Path(td) / "sae_ramp_by_level.csv"
        if not lvl_csv.exists():
            return []
        with open(lvl_csv) as fh:
            for r in csv.DictReader(fh):
                rows.append({"topo": topo, "N": n, **r})
    return rows


def read_cell(cell: Path):
    """Master-row metrics for one cell. None fields where data is absent (STALL)."""
    name = cell.name
    m = re.match(r"full-(\w+?)-n(\d+)$", name)
    if not m:
        return None
    topo, n = m.group(1), int(m.group(2))
    row = {"topo": topo, "N": n, "cell": name, "nodes": "", "edges": "",
           "smoke_rc": "", "match_pct": "", "p429_pct": "", "p_other_err_pct": "",
           "total_requests": "", "enc_p50_ms": "", "enc_p95_ms": "", "enc_p99_ms": "",
           "dec_p50_ms": "", "dec_p95_ms": "", "dec_p99_ms": "",
           "solve_count": "", "solve_median_s": "", "solve_max_s": "",
           "buf_fill_kps": "", "buf_peak_M": ""}

    sr = cell / "smoke_rc"
    if sr.exists():
        row["smoke_rc"] = sr.read_text().strip()

    plan = cell / "plan.json"
    if plan.exists():
        try:
            meta = json.loads(plan.read_text()).get("meta", {})
            row["nodes"] = meta.get("nodes", meta.get("n_nodes", ""))
            row["edges"] = meta.get("edges", meta.get("n_edges", ""))
        except Exception:
            pass

    sj = cell / "summary.json"
    if sj.exists():
        try:
            s = json.loads(sj.read_text())
            tot = s.get("total_requests", 0) or 0
            row["total_requests"] = tot
            if tot:
                e = s.get("http_errors_from_server", {}) or {}
                e429 = sum(v for k, v in e.items() if "429" in k)
                eoth = sum(v for k, v in e.items() if "429" not in k)
                eoth += sum((s.get("client_errors", {}) or {}).values())
                row["match_pct"] = round(100.0 * s.get("match", 0) / tot, 3)
                row["p429_pct"] = round(100.0 * e429 / tot, 3)
                row["p_other_err_pct"] = round(100.0 * eoth / tot, 3)
            for plane, key in (("enc", "enc_latency_ms"), ("dec", "dec_latency_ms")):
                lat = s.get(key, {}) or {}
                for p in ("p50", "p95", "p99"):
                    row[f"{plane}_{p}_ms"] = lat.get(p, "")
        except Exception:
            pass

    sc, smed, smax = solve_times(cell)
    row["solve_count"] = sc
    row["solve_median_s"] = round(smed, 1) if smed is not None else ""
    row["solve_max_s"] = round(smax, 1) if smax is not None else ""
    bfk, bpk = buffer_stats(cell)
    row["buf_fill_kps"] = round(bfk / 1000.0, 2) if bfk is not None else ""
    row["buf_peak_M"] = round(bpk, 3) if bpk is not None else ""
    return row


def grid(rows, fmt):
    by = {(r["topo"], r["N"]): r for r in rows}
    lines = ["| topo | " + " | ".join(f"N={n}" for n in NS) + " |",
             "|---|" + "|".join("---" for _ in NS) + "|"]
    for t in TOPOS:
        cells = []
        for n in NS:
            r = by.get((t, n))
            cells.append(fmt(r) if r else "·")
        lines.append(f"| {t} | " + " | ".join(cells) + " |")
    return "\n".join(lines)


def main():
    OUT.mkdir(exist_ok=True)
    cells = sorted(p for p in RESULTS.glob("full-*-n*") if (p / "DONE").exists() and p.is_dir())
    rows = [r for r in (read_cell(c) for c in cells) if r]
    rows.sort(key=lambda r: (r["N"], TOPOS.index(r["topo"]) if r["topo"] in TOPOS else 9))

    # cells.csv
    if rows:
        with open(OUT / "cells.csv", "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=list(rows[0].keys()))
            w.writeheader(); w.writerows(rows)

    # ramp_levels.csv (long)
    lvl_rows = []
    for c in cells:
        mm = re.match(r"full-(\w+?)-n(\d+)$", c.name)
        if mm:
            lvl_rows += ramp_levels(c, mm.group(1), int(mm.group(2)))
    if lvl_rows:
        with open(OUT / "ramp_levels.csv", "w", newline="") as fh:
            w = csv.DictWriter(fh, fieldnames=list(lvl_rows[0].keys()))
            w.writeheader(); w.writerows(lvl_rows)

    # timeseries.csv (long: buffers over time)
    with open(OUT / "timeseries.csv", "w", newline="") as fh:
        w = csv.writer(fh)
        w.writerow(["topo", "N", "t_s", "total_enc_keys", "active_pairs"])
        for c in cells:
            mm = re.match(r"full-(\w+?)-n(\d+)$", c.name)
            bf = c / "buffer_fill.csv"
            if not (mm and bf.exists()):
                continue
            with open(bf) as bfh:
                for r in csv.DictReader(bfh):
                    w.writerow([mm.group(1), int(mm.group(2)),
                                r.get("t_s", ""), r.get("total_enc_keys", ""), r.get("active_pairs", "")])

    # MATRIX.md
    def fmt(r):
        if r.get("smoke_rc") not in ("0", 0):
            return "STALL"
        if r.get("match_pct") == "":
            return "?"
        return f"{r['match_pct']:.0f}/{r['p429_pct']:.0f}"
    md = [f"# Rejilla de saturación — 5 topos × N{{10..100}} ({len(rows)} celdas)", "",
          "Clarabel, rampa SAE 500→16000, supply natural. Celda = `match% / 429%`; "
          "`STALL` = el SDN no resolvió a tiempo; `·` = no ejecutada.", "",
          "## match % / 429 %", grid(rows, fmt)]
    (OUT / "MATRIX.md").write_text("\n".join(md) + "\n")
    print(f"[full_table] {len(rows)} cells → {OUT.relative_to(REPO)}/"
          "{cells.csv,ramp_levels.csv,timeseries.csv,MATRIX.md}")


if __name__ == "__main__":
    main()
