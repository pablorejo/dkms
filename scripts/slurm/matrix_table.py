#!/usr/bin/env python3
"""Aggregate the saturation-matrix cells into a 5×5 table.

Scans ``tests/results/matrix-<topo>-n<N>/`` dirs, reads each cell's
``summary.json`` + ``smoke_rc``, and emits markdown grids (match%, 429%, smoke
gate / SDN status) over rows = topology, cols = N. Cells not yet run show ``·``.

Works on partial progress (call it any time during the campaign) and is the
input for the final ANALYSIS synthesis.

Usage:  matrix_table.py [--results tests/results] [--out tests/results/matrix-n20-60]
"""
from __future__ import annotations

import argparse
import csv
import json
import re
from pathlib import Path

TOPOS = ["ba", "er", "ba2", "rgg", "secoqc"]
NS = [20, 30, 40, 50, 60]
CELL_RE = re.compile(r"^matrix-(ba2|ba|er|rgg|secoqc)-n(\d+)$")


def load_cell(d: Path) -> dict | None:
    smoke = (d / "smoke_rc").read_text().strip() if (d / "smoke_rc").is_file() else "?"
    summ = d / "summary.json"
    out = {"smoke_rc": smoke, "match_pct": None, "total": None, "p429_pct": None,
           "done": (d / "DONE").is_file()}
    if summ.is_file():
        try:
            j = json.loads(summ.read_text())
            tot = j.get("total_requests", 0) or 0
            errs = j.get("http_errors_from_server", {}) or {}
            n429 = sum(v for k, v in errs.items() if "429" in str(k))
            out["match_pct"] = j.get("match_pct")
            out["total"] = tot
            out["p429_pct"] = round(100.0 * n429 / tot, 2) if tot else None
        except Exception:  # noqa: BLE001
            pass
    return out


def grid(cells: dict, field: str, fmt) -> list[str]:
    lines = ["| topo | " + " | ".join(f"N={n}" for n in NS) + " |",
             "|" + "---|" * (len(NS) + 1)]
    for t in TOPOS:
        row = [t]
        for n in NS:
            c = cells.get((t, n))
            row.append(fmt(c[field]) if c and c.get(field) is not None else "·")
        lines.append("| " + " | ".join(row) + " |")
    return lines


def smoke_grid(cells: dict) -> list[str]:
    def sym(c):
        if not c:
            return "·"
        if not c["done"]:
            return "…"
        return "ok" if c["smoke_rc"] == "0" else f"STALL({c['smoke_rc']})"
    lines = ["| topo | " + " | ".join(f"N={n}" for n in NS) + " |",
             "|" + "---|" * (len(NS) + 1)]
    for t in TOPOS:
        lines.append("| " + " | ".join([t] + [sym(cells.get((t, n))) for n in NS]) + " |")
    return lines


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", type=Path, default=Path("tests/results"))
    ap.add_argument("--out", type=Path, default=Path("tests/results/matrix-n20-60"))
    args = ap.parse_args()

    cells: dict[tuple[str, int], dict] = {}
    for d in sorted(args.results.glob("matrix-*-n*")):
        m = CELL_RE.match(d.name)
        if not m:
            continue
        cells[(m.group(1), int(m.group(2)))] = load_cell(d)

    done = sum(1 for c in cells.values() if c["done"])
    md = [f"# Saturation matrix — 5 topos × N{{20,30,40,50,60}} ({done}/25 cells)\n",
          "Natural SDN supply (MCMCF-λ, qd-alpha 0, ba40-ramp recipe). "
          "Cells: `·`=not run, `…`=running, `ok`=SDN solved+ramp, `STALL`=smoke failed (SDN LP didn't push rates → ramp skipped).\n",
          "## Smoke / SDN status", *smoke_grid(cells), "",
          "## match % (enc→dec byte-match)", *grid(cells, "match_pct", lambda v: f"{v:.1f}"), "",
          "## HTTP 429 % (DKMS backpressure)", *grid(cells, "p429_pct", lambda v: f"{v:.1f}"), "",
          "## total requests", *grid(cells, "total", lambda v: f"{v/1e6:.2f}M"), ""]
    text = "\n".join(md)
    print(text)

    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "MATRIX.md").write_text(text)
    with (args.out / "matrix.csv").open("w", newline="") as f:
        w = csv.writer(f)
        w.writerow(["topo", "N", "done", "smoke_rc", "match_pct", "p429_pct", "total"])
        for t in TOPOS:
            for n in NS:
                c = cells.get((t, n))
                if c:
                    w.writerow([t, n, c["done"], c["smoke_rc"], c["match_pct"], c["p429_pct"], c["total"]])
    print(f"\n[matrix_table] wrote {args.out}/MATRIX.md + matrix.csv ({done}/25 done)")


if __name__ == "__main__":
    main()
