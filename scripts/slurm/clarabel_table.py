#!/usr/bin/env python3
"""Comparison table: microlp baseline (matrix-*) vs Clarabel (clarabel-*).

Reads tests/results/matrix-n20-60/matrix.csv (the "antes" microlp matrix) and
each tests/results/clarabel-<fam>-n<N>/summary.json (the "después" cells), and
writes tests/results/clarabel-n40-60/COMPARISON.md. Cells the baseline solved
(N=20/30 and ba-n40) are shown for context from the baseline only.
"""
import csv
import json
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
RESULTS = REPO / "tests/results"
OUT_DIR = RESULTS / "clarabel-n40-60"

FAMS = ["ba", "er", "ba2", "rgg", "secoqc"]
SIZES = [20, 30, 40, 50, 60]


def pct_pair(summary_path):
    """match% / 429% a un decimal desde los CONTEOS crudos del summary.json
    (match_pct/p429_pct ya vienen redondeados a 2 decimales — re-redondear
    desde ahí desplaza el decimal: 8.0494→8.05→8.1)."""
    s = json.loads(summary_path.read_text())
    total = s["total_requests"]
    e429 = sum(v for k, v in s.get("http_errors_from_server", {}).items() if "429" in k)
    return f"{100 * s['match'] / total:.1f}% / {100 * e429 / total:.1f}%"


def load_baseline():
    base = {}
    with open(RESULTS / "matrix-n20-60/matrix.csv") as f:
        for row in csv.DictReader(f):
            key = (row["topo"], int(row["N"]))
            sj = RESULTS / f"matrix-{row['topo']}-n{row['N']}/summary.json"
            if row["smoke_rc"] == "0" and sj.exists():
                base[key] = pct_pair(sj)
            elif row["smoke_rc"] == "1":
                base[key] = "STALL"
            else:
                base[key] = "?"
    return base


def load_clarabel():
    cla = {}
    for d in RESULTS.glob("clarabel-*-n*"):
        if not (d / "DONE").exists() or d.name.endswith("solverburst"):
            continue
        fam, n = d.name.replace("clarabel-", "").rsplit("-n", 1)
        cla[(fam, int(n))] = pct_pair(d / "summary.json")
    return cla


def main():
    base, cla = load_baseline(), load_clarabel()
    lines = [
        "# microlp vs Clarabel — match% / 429% por celda",
        "",
        "Misma receta en todas las celdas (rampa 500→16000 SAEs, Poisson λ=1,",
        "supply natural MCMCF-λ); única variable: `SDN_SOLVER`. `STALL` = el LP",
        "nunca completó un solve (smoke 0/50, buffers a cero, rampa no ejecutada).",
        "Las celdas clarabel-* corren con el guard de coalescencia del debouncer",
        "(ver ANALYSIS.md: er-n60 v1 lo motivó).",
        "",
        "| topo | N | microlp (antes) | Clarabel (después) |",
        "|---|---|---|---|",
    ]
    for fam in FAMS:
        for n in SIZES:
            b = base.get((fam, n), "·")
            c = cla.get((fam, n), "—" if b != "STALL" else "·")
            lines.append(f"| {fam} | {n} | {b} | {c} |")
    OUT_DIR.mkdir(exist_ok=True)
    out = OUT_DIR / "COMPARISON.md"
    out.write_text("\n".join(lines) + "\n")
    print(f"[clarabel_table] wrote {out.relative_to(REPO)}")


if __name__ == "__main__":
    main()
