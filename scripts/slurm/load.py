#!/usr/bin/env python3
"""Sustained / ramp load driver against a running deployment (plan.json).

Spawns worker threads that issue ETSI-014 ``enc_keys`` over mTLS to each SAE's
home DKMS, optionally ramping concurrency over time, and records every request
(epoch, sae, status, latency_ms) to a CSV. Prints a summary: throughput,
HTTP-status histogram, and latency percentiles. Stdlib only.

Used for the scale/characterization steps (N=10→40→100): drive it from rank 0
of a KEEP=1 deployment, point at the generated plan, collect the CSV under
tests/results/ (or $LUSTRE).
"""
from __future__ import annotations

import argparse
import http.client
import json
import ssl
import threading
import time
from collections import Counter
from pathlib import Path
from urllib.parse import urlparse


class Worker(threading.Thread):
    def __init__(self, assign: dict, size_bits: int, rate: float, stop_at: float,
                 timeout: float, sink: list):
        super().__init__(daemon=True)
        self.a = assign
        self.size = size_bits
        self.interval = (1.0 / rate) if rate > 0 else 0.0
        self.stop_at = stop_at
        self.timeout = timeout
        self.sink = sink
        u = urlparse(assign["url"])
        self.host, self.port = u.hostname, u.port
        ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        ctx.load_cert_chain(certfile=assign["cert"], keyfile=assign["key"])
        self.ctx = ctx
        self.path = f"/api/v1/keys/{assign['slave_sae']}/enc_keys"
        self.body = json.dumps({"number": 1, "size": size_bits})
        self.local = []

    def run(self) -> None:
        conn = None
        while time.time() < self.stop_at:
            t0 = time.time()
            status = "ERR"
            try:
                if conn is None:
                    conn = http.client.HTTPSConnection(self.host, self.port,
                                                       context=self.ctx, timeout=self.timeout)
                conn.request("POST", self.path, body=self.body,
                             headers={"Content-Type": "application/json"})
                resp = conn.getresponse()
                resp.read()
                status = resp.status
                if status >= 500 or status == 408:
                    conn.close(); conn = None
            except Exception:  # noqa: BLE001
                status = "ERR"
                if conn is not None:
                    conn.close(); conn = None
            dt = (time.time() - t0) * 1000.0
            self.local.append((round(t0, 3), self.a["sae_id"], status, round(dt, 2)))
            if self.interval:
                sleep = self.interval - (time.time() - t0)
                if sleep > 0:
                    time.sleep(sleep)
        self.sink.extend(self.local)


def pct(sorted_vals: list, p: float) -> float:
    if not sorted_vals:
        return 0.0
    k = min(len(sorted_vals) - 1, int(p / 100.0 * len(sorted_vals)))
    return sorted_vals[k]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan", required=True)
    ap.add_argument("--out", required=True, help="CSV output path")
    ap.add_argument("--duration", type=float, default=30.0)
    ap.add_argument("--workers-per-sae", type=int, default=1)
    ap.add_argument("--rate", type=float, default=20.0, help="req/s per worker (0 = as fast as possible)")
    ap.add_argument("--ramp", type=float, default=0.0,
                    help="if >0, stagger worker start over this many seconds")
    ap.add_argument("--size-bits", type=int, default=None)
    ap.add_argument("--timeout", type=float, default=10.0)
    args = ap.parse_args()

    plan = json.loads(Path(args.plan).read_text())
    size = args.size_bits or plan["meta"]["key_bits"]
    assigns = plan["sae_assign"]
    stop_at = time.time() + args.duration
    sink: list = []
    workers = []
    for a in assigns:
        for _ in range(args.workers_per_sae):
            workers.append(Worker(a, size, args.rate, stop_at, args.timeout, sink))

    n = len(workers)
    print(f"[load] {n} workers ({len(assigns)} SAEs x {args.workers_per_sae}), "
          f"rate {args.rate}/s/worker, duration {args.duration}s, size {size}b")
    stagger = (args.ramp / n) if (args.ramp > 0 and n) else 0.0
    for w in workers:
        w.start()
        if stagger:
            time.sleep(stagger)
    for w in workers:
        w.join()

    sink.sort()
    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    with open(args.out, "w") as f:
        f.write("epoch,sae,status,latency_ms\n")
        for row in sink:
            f.write(f"{row[0]},{row[1]},{row[2]},{row[3]}\n")

    status_hist = Counter(str(r[2]) for r in sink)
    oks = [r[3] for r in sink if r[2] == 200]
    oks.sort()
    total = len(sink)
    wall = (sink[-1][0] - sink[0][0]) if total > 1 else args.duration
    thr = status_hist.get("200", 0) / wall if wall > 0 else 0.0
    print(f"[load] requests={total} over ~{wall:.1f}s")
    print(f"[load] status={dict(status_hist)}")
    print(f"[load] 200-throughput={thr:.1f}/s")
    if oks:
        print(f"[load] latency_ms p50={pct(oks,50):.1f} p90={pct(oks,90):.1f} "
              f"p99={pct(oks,99):.1f} max={oks[-1]:.1f}")
    print(f"[load] csv → {args.out}")


if __name__ == "__main__":
    main()
