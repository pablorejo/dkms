#!/usr/bin/env python3
"""Extract one matrix-cell's headline metrics for the campaign status table.

Reads the collected ETSI-014 round-trip summary (``summary.json``) plus the
run's ``smoke_rc`` and prints a single CSV row fragment:

    smoke_rc,match_pct,total_requests,p429_pct,note

- ``smoke_rc``  : rank-0 smoke gate exit code (0 = data plane OK; non-0 = SDN
                  never pushed rates / mTLS broken → ramp was skipped).
- ``match_pct`` : global enc→dec byte-match percentage (the saturation signal).
- ``p429_pct``  : share of requests the DKMS axum backpressured with HTTP 429.
- ``note``      : human hint (``ok`` / ``smoke-fail-no-ramp`` / ``no-summary``).

Usage:  matrix_extract.py --results <tests/results/NAME> --run <LUSTRE/runs/NAME>
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def find_summary(results: Path, run: Path) -> Path | None:
    for cand in (results / "summary.json", run / "rt" / "summary.json"):
        if cand.is_file():
            return cand
    return None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--results", required=True, type=Path)
    ap.add_argument("--run", required=True, type=Path)
    args = ap.parse_args()

    smoke_rc = "?"
    rc_file = args.run / "smoke_rc"
    if rc_file.is_file():
        smoke_rc = rc_file.read_text().strip() or "?"

    summ = find_summary(args.results, args.run)
    if summ is None:
        note = "smoke-fail-no-ramp" if smoke_rc not in ("0", "?") else "no-summary"
        print(f"{smoke_rc},,,,{note}")
        return

    try:
        d = json.loads(summ.read_text())
    except Exception as e:  # noqa: BLE001
        print(f"{smoke_rc},,,,bad-summary:{type(e).__name__}")
        return

    match_pct = d.get("match_pct", "")
    total = d.get("total_requests", 0) or 0
    # sum every server-side error whose label mentions 429
    errs = d.get("http_errors_from_server", {}) or {}
    n429 = sum(v for k, v in errs.items() if "429" in str(k))
    p429 = round(100.0 * n429 / total, 2) if total else ""
    note = "ok" if smoke_rc == "0" else "ramp-ran-smoke-nonzero"
    print(f"{smoke_rc},{match_pct},{total},{p429},{note}")


if __name__ == "__main__":
    main()
