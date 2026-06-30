#!/usr/bin/env python3
"""Mine RAW runtime metrics from the campaign deployment logs.

For every camp/<cell>/logs dir: parse the SDN `MCMCF-λ recomputed` line
(elapsed_ms, lambda, n_commodities, n_edges) and the per-DKMS final
`generator.state` enc fill (mean + coefficient of variation = fill sync).
Cell params come from the dir name (topo, N, pqc, solver/dual). One JSON object
per cell to the output file. Analysis/plots are separate (plot_seclevels.py).
"""
import glob
import json
import os
import re
import statistics
import sys

CAMP = os.environ.get("CAMP", "/mnt/lustre/scratch/nlsas/home/uvi/et/dca/dkms-build/camp")
OUT = sys.argv[1] if len(sys.argv) > 1 else \
    "/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust/tests/results/seclevels-campaign/data/runtime_metrics.jsonl"
os.makedirs(os.path.dirname(OUT), exist_ok=True)
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def parse_name(name):
    m = re.search(r"_(er|ba|rgg|mesh|star|ring)_n(\d+)_pqc([0-9.]+)", name)
    if not m:
        return None
    d = re.search(r"_d(\d)", name)
    return dict(topo=m.group(1), N=int(m.group(2)), pqc=float(m.group(3)),
                dual=int(d.group(1)) if d else 1)


rows = 0
with open(OUT, "w") as fh:
    for d in sorted(glob.glob(os.path.join(CAMP, "*"))):
        L = os.path.join(d, "logs")
        if not os.path.isdir(L):
            continue
        p = parse_name(os.path.basename(d))
        if not p:
            continue
        # SDN recompute (last successful solve)
        solve_ms = lam = ncomm = nedg = None
        sf = os.path.join(L, "sdn.log")
        if os.path.exists(sf):
            for line in open(sf, errors="ignore"):
                if "MCMCF-λ recomputed" in line or "MCMCF-" in line and "recomputed" in line:
                    s = ANSI.sub("", line)
                    def g(k):
                        mm = re.search(k + r"=([0-9.eE-]+)", s)
                        return float(mm.group(1)) if mm else None
                    solve_ms = g("elapsed_ms") or solve_ms
                    lam = g("lambda") or lam
                    ncomm = g("n_commodities") or ncomm
                    nedg = g("n_edges") or nedg
        # per-DKMS final enc fill
        fills = []
        for f in glob.glob(os.path.join(L, "dkms-*.log")):
            byt = {}
            for line in open(f, errors="ignore"):
                if "generator.state" in line:
                    s = ANSI.sub("", line)
                    mm = re.search(r"enc=(\d+)", s)
                    if mm:
                        byt.setdefault(s[:19], 0)
                        byt[s[:19]] += int(mm.group(1))
            if byt:
                fills.append(byt[sorted(byt)[-1]])
        # PQC handshakes
        hs = 0
        for f in glob.glob(os.path.join(L, "qkc-*.log")):
            hs += sum(1 for ln in open(f, errors="ignore") if "pqc.handshake.established" in ln)
        row = dict(cell=os.path.basename(d), **p,
                   solve_ms=solve_ms, lam=lam, n_commodities=ncomm, n_edges=nedg,
                   n_dkms_filled=sum(1 for x in fills if x > 0), n_dkms=len(fills),
                   fill_mean=statistics.mean(fills) if fills else 0,
                   fill_cv=(100 * statistics.pstdev(fills) / statistics.mean(fills))
                   if fills and statistics.mean(fills) > 0 else None,
                   pqc_handshakes=hs)
        fh.write(json.dumps(row) + "\n")
        rows += 1
print(f"wrote {rows} runtime rows -> {OUT}")
