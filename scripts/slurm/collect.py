#!/usr/bin/env python3
"""Collect a run's results from its (LUSTRE) working dir into tests/results/<name>/.

The live deployment runs on $LUSTRE (configs hold absolute $LUSTRE paths and the
binaries live there). This copies the *results* back into the repo's
`tests/results/` — which is gitignored — keeping inode use tiny on the
quota-limited $HOME: a few loose human-readable artifacts plus the bulky/numerous
per-process logs (and regenerable config tree) compressed into single tarballs.

  python3 collect.py --run $LUSTRE/dkms-runs/er40 [--name er40]
"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

# Loose, human-readable artifacts worth keeping uncompressed (few inodes).
LOOSE = ["deploy.out", "plan.json", "smoke.txt", "smoke_rc", "load.txt",
         "load.csv", "nodes.txt", "hosts.txt"]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True, help="run working dir (on $LUSTRE)")
    ap.add_argument("--name", default=None, help="campaign name (default: run dir basename)")
    ap.add_argument("--results", default=None, help="tests/results dir (default: repo/tests/results)")
    args = ap.parse_args()

    run = Path(args.run)
    if not run.exists():
        print(f"[collect] run dir not found: {run}", file=sys.stderr)
        sys.exit(1)
    name = args.name or run.name
    repo = Path(__file__).resolve().parents[2]
    results = Path(args.results) if args.results else (repo / "tests" / "results")
    dest = results / name
    dest.mkdir(parents=True, exist_ok=True)

    # 1) loose artifacts (some live at the run root, some under logs/)
    kept = []
    for f in LOOSE:
        for src in (run / f, run / "logs" / f):
            if src.exists():
                shutil.copy2(src, dest / f)
                kept.append(f)
                break

    # 2) analyzer → ANALYSIS.md (text tables) + buffer_fill.csv + load_timeseries.csv
    try:
        out = subprocess.run(
            [sys.executable, str(repo / "scripts/slurm/analyze_run.py"), str(run),
             "--csv-dir", str(dest)],
            capture_output=True, text=True, timeout=120)
        (dest / "ANALYSIS.md").write_text(
            f"# {name}\n\n```\n{out.stdout}\n{out.stderr}\n```\n")
        kept.append("ANALYSIS.md")
        for c in ("buffer_fill.csv", "load_timeseries.csv"):
            if (dest / c).exists():
                kept.append(c)
    except Exception as e:  # noqa: BLE001
        (dest / "ANALYSIS.md").write_text(f"# {name}\n\nanalyzer failed: {e}\n")

    # 3) plots (PNG) — matplotlib lives in the system python3 on CESGA
    try:
        subprocess.run(
            [sys.executable, str(repo / "scripts/slurm/plot_run.py"),
             "--csv-dir", str(dest), "--raw-load", str(run / "logs" / "load.csv"),
             "--out", str(dest / "plots"), "--name", name],
            capture_output=True, text=True, timeout=120)
        pngs = sorted(p.name for p in (dest / "plots").glob("*.png"))
        kept += [f"plots/{p}" for p in pngs]
    except Exception as e:  # noqa: BLE001
        print(f"[collect] plotting failed: {e}", file=sys.stderr)

    # 3b) ETSI-014 round-trip campaign results (the n_20-comparable artifacts):
    #     summary.json + the 8 plots loose, big requests.csv → tarball.
    rt = run / "rt"
    if rt.is_dir():
        if (rt / "summary.json").exists():
            shutil.copy2(rt / "summary.json", dest / "summary.json")
            kept.append("summary.json")
        for png in sorted((rt / "plots").glob("*.png")) if (rt / "plots").is_dir() else []:
            (dest / "plots").mkdir(exist_ok=True)
            shutil.copy2(png, dest / "plots" / png.name)
            kept.append(f"plots/{png.name}")
        try:
            import tarfile as _tf
            with _tf.open(dest / "roundtrip.tar.gz", "w:gz") as t:
                for f in ("requests.csv", "aggregate.log", "plot.log"):
                    if (rt / f).exists():
                        t.add(rt / f, arcname=f)
                for wl in sorted(rt.glob("worker-*.log")):
                    t.add(wl, arcname=wl.name)
            kept.append("roundtrip.tar.gz")
        except Exception as e:  # noqa: BLE001
            print(f"[collect] roundtrip tar failed: {e}", file=sys.stderr)

    # 4) bulky/numerous → single tarballs (1 inode each)
    for sub, arc in [("logs", "logs.tar.gz"),
                     (None, "configs.tar.gz")]:  # configs = topology+sites+tls
        try:
            if sub == "logs" and (run / "logs").is_dir():
                with tarfile.open(dest / arc, "w:gz") as t:
                    t.add(run / "logs", arcname="logs")
                kept.append(arc)
            elif sub is None:
                members = [run / d for d in ("topology", "sites", "tls") if (run / d).is_dir()]
                if members:
                    with tarfile.open(dest / arc, "w:gz") as t:
                        for m in members:
                            t.add(m, arcname=m.name)
                    kept.append(arc)
        except Exception as e:  # noqa: BLE001
            print(f"[collect] tar {arc} failed: {e}", file=sys.stderr)

    print(f"[collect] {name}: {len(kept)} artifacts → {dest}")
    print(f"          {', '.join(kept)}")


if __name__ == "__main__":
    main()
