#!/usr/bin/env python3
"""Compare ETSI-014 round-trip campaigns (summary.json) across runs — e.g. the
prior resultados_definitivos_n_20/* vs the new larger-N runs under tests/results/.

  python3 compare_rt.py --label n20-er resultados_definitivos_n_20/er \
                        --label er40   tests/results/etsi014-er40-sat ...
  (or: --auto  to pick up resultados_definitivos_n_20/* + tests/results/etsi014-*)

Writes a markdown table + summary_compare.csv + (if matplotlib) a grouped bar
chart of match% / enc-p50 / enc-p99 to tests/results/COMPARE_rt.{md,csv,png}.
"""
from __future__ import annotations

import argparse
import csv
import json
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]


def load(d: Path):
    f = d / "summary.json"
    if not f.exists():
        return None
    try:
        return json.loads(f.read_text())
    except Exception:  # noqa: BLE001
        return None


def g(s, *path, default=""):
    cur = s
    for p in path:
        if not isinstance(cur, dict) or p not in cur:
            return default
        cur = cur[p]
    return cur


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", action="append", nargs=2, metavar=("NAME", "DIR"), default=[])
    ap.add_argument("--auto", action="store_true")
    ap.add_argument("--out", default=str(REPO / "tests" / "results"))
    args = ap.parse_args()

    runs = list(args.label)
    if args.auto:
        for d in sorted((REPO / "resultados_definitivos_n_20").glob("*")):
            if (d / "summary.json").exists():
                runs.append([f"n20-{d.name}", str(d)])
        for d in sorted((REPO / "tests" / "results").glob("etsi014-*")):
            if (d / "summary.json").exists():
                runs.append([d.name, str(d)])

    rows = []
    for name, d in runs:
        s = load(Path(d))
        if not s:
            continue
        rows.append({
            "run": name,
            "workers": g(s, "n_workers"),
            "total": g(s, "total_requests"),
            "enc_ok_pct": g(s, "enc_ok_pct"),
            "match_pct": g(s, "match_pct"),
            "enc_p50": g(s, "enc_latency_ms", "p50"),
            "enc_p99": g(s, "enc_latency_ms", "p99"),
            "dec_p50": g(s, "dec_latency_ms", "p50"),
            "dec_p99": g(s, "dec_latency_ms", "p99"),
            "enc_429": g(s, "http_errors_from_server", "enc HTTP 429", default=0),
        })

    out = Path(args.out); out.mkdir(parents=True, exist_ok=True)
    with open(out / "summary_compare.csv", "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0].keys()) if rows else ["run"])
        w.writeheader(); w.writerows(rows)

    md = ["# ETSI-014 round-trip — campaign comparison", "",
          "| Run | Workers | Total req | enc_ok % | match % | enc p50 ms | enc p99 ms | dec p50 ms | dec p99 ms | enc 429 |",
          "|-----|---------|-----------|----------|---------|------------|------------|------------|------------|---------|"]
    for r in rows:
        md.append("| {run} | {workers} | {total} | {enc_ok_pct} | {match_pct} | {enc_p50} | {enc_p99} | {dec_p50} | {dec_p99} | {enc_429} |".format(**r))
    md += ["", "n20-* = prior definitive results (N=20, orchestrator/EKS, 30 workers × 500 pairs).",
           "etsi014-* = new binaries-only Slurm runs at larger N (same round-trip methodology)."]
    (out / "COMPARE_rt.md").write_text("\n".join(md) + "\n")

    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        names = [r["run"] for r in rows]
        x = range(len(names))
        fig, (a1, a2) = plt.subplots(1, 2, figsize=(max(8, 1.2 * len(names)), 4.5))
        a1.bar(x, [float(r["match_pct"] or 0) for r in rows], color="seagreen")
        a1.set_xticks(list(x)); a1.set_xticklabels(names, rotation=40, ha="right")
        a1.set_ylabel("match %"); a1.set_title("Round-trip match % per campaign"); a1.grid(True, axis="y", alpha=0.3)
        a2.bar([i - 0.2 for i in x], [float(r["enc_p50"] or 0) for r in rows], width=0.4, label="enc p50", color="steelblue")
        a2.bar([i + 0.2 for i in x], [float(r["enc_p99"] or 0) for r in rows], width=0.4, label="enc p99", color="firebrick")
        a2.set_xticks(list(x)); a2.set_xticklabels(names, rotation=40, ha="right")
        a2.set_ylabel("latency (ms)"); a2.set_yscale("log"); a2.set_title("enc_keys latency"); a2.legend(); a2.grid(True, axis="y", alpha=0.3)
        fig.tight_layout(); fig.savefig(out / "COMPARE_rt.png", dpi=120); plt.close(fig)
    except Exception as e:  # noqa: BLE001
        print(f"[compare] plot skipped: {e}")

    print(f"[compare] {len(rows)} campaigns → {out/'COMPARE_rt.md'} + summary_compare.csv + COMPARE_rt.png")
    print("\n".join(md[2:4 + len(rows)]))


if __name__ == "__main__":
    main()
