#!/usr/bin/env python3
"""ETSI-014 round-trip load worker — reproduces the per-worker requests.csv that
the etsi014_loadtest harness produced for the N=20 "definitive results", but
driven against a binaries-only Slurm deployment (no orchestrator/ingress).

For each active SAE pair, at a Poisson-ish rate (uniform(0,2/λ) inter-arrival):
  enc: POST {enc_url}  {"number":1,"size":BITS}        → key_ID, key_master
  dec: POST {dec_url}  {"key_IDs":[{"key_ID":key_ID}]} → key_slave ; match=(==)
Writes <out>/worker-<id>/requests.csv with the canonical 13 columns:
  t_emit,pair_id,master_sae,slave_sae,master_host_id,slave_host_id,key_id,
  enc_ms,dec_ms,ok_enc,ok_dec,match,err
so aggregate_workers.py + plot_{roundtrip,error_breakdown,match_vs_429}.py work
verbatim. Stdlib only (thread-per-pair; no aiohttp). mTLS = client cert + CERT_NONE.

Ramp: start with START_PAIRS active, add STEP_PAIRS every INTERVAL s up to the
worker's pair count, then HOLD. This worker owns pairs[i] where i % WORKERS == ID.
"""
from __future__ import annotations

import argparse
import csv
import http.client
import json
import queue
import random
import ssl
import threading
import time
from pathlib import Path
from urllib.parse import urlparse

FIELDS = ["t_emit", "pair_id", "master_sae", "slave_sae", "master_host_id",
          "slave_host_id", "key_id", "enc_ms", "dec_ms", "ok_enc", "ok_dec",
          "match", "err"]


def ctx_for(pem: str) -> ssl.SSLContext:
    c = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    c.load_cert_chain(certfile=pem)  # combined cert+key PEM
    return c


def post(conn: http.client.HTTPSConnection, path: str, body: str):
    conn.request("POST", path, body=body, headers={"Content-Type": "application/json"})
    r = conn.getresponse()
    data = r.read()
    return r.status, data


def pair_loop(pair: dict, lam: float, size: int, timeout: float,
              q: "queue.Queue", stop: threading.Event, poisson: bool = False) -> None:
    enc = urlparse(pair["enc_url"]); dec = urlparse(pair["dec_url"])
    cm = ctx_for(pair["master_pem"]); cs = ctx_for(pair["slave_pem"])
    enc_body = json.dumps({"number": 1, "size": size})
    mean = 1.0 / lam
    # inter-arrival: true Poisson process (exponential, mean 1/lam) when --poisson,
    # else the legacy uniform(0, 2/lam) approximation (same mean rate, lower variance).
    def wait() -> float:
        return random.expovariate(lam) if poisson else random.uniform(0, 2 * mean)
    sm = ss = None
    time.sleep(random.expovariate(lam) if poisson else random.uniform(0, mean))  # initial jitter
    while not stop.is_set():
        time.sleep(wait())
        if stop.is_set():
            break
        t_emit = time.time()
        key_id, enc_ms, dec_ms = "", 0.0, 0.0
        ok_enc = ok_dec = match = 0
        err = ""
        km = None
        try:
            if sm is None:
                sm = http.client.HTTPSConnection(enc.hostname, enc.port, context=cm, timeout=timeout)
            t0 = time.perf_counter()
            st, data = post(sm, f"{enc.path}", enc_body)
            enc_ms = (time.perf_counter() - t0) * 1000.0
            if st != 200:
                err = f"enc HTTP {st}: {data[:120].decode('utf-8','replace')}"
            else:
                k = json.loads(data)["keys"][0]
                key_id, km, ok_enc = k["key_ID"], k["key"], 1
        except Exception as e:  # noqa: BLE001
            err = f"enc exc: {e}"
            if sm:
                sm.close(); sm = None
        if ok_enc:
            dec_body = json.dumps({"key_IDs": [{"key_ID": key_id}]})
            try:
                if ss is None:
                    ss = http.client.HTTPSConnection(dec.hostname, dec.port, context=cs, timeout=timeout)
                t0 = time.perf_counter()
                st, data = post(ss, f"{dec.path}", dec_body)
                dec_ms = (time.perf_counter() - t0) * 1000.0
                if st != 200:
                    err = f"dec HTTP {st}: {data[:120].decode('utf-8','replace')}"
                else:
                    ks = json.loads(data)["keys"][0]["key"]
                    ok_dec = 1
                    match = 1 if ks == km else 0
            except Exception as e:  # noqa: BLE001
                err = f"dec exc: {e}"
                if ss:
                    ss.close(); ss = None
        q.put((f"{t_emit:.6f}", pair["pair_id"], pair["master_sae"], pair["slave_sae"],
               pair["master_host_id"], pair["slave_host_id"], key_id,
               f"{enc_ms:.2f}", f"{dec_ms:.2f}", ok_enc, ok_dec, match, err))


def writer(path: Path, q: "queue.Queue", stop: threading.Event) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", newline="") as f:
        w = csv.writer(f)
        w.writerow(FIELDS)
        n = 0
        while not (stop.is_set() and q.empty()):
            try:
                row = q.get(timeout=0.5)
            except queue.Empty:
                continue
            w.writerow(row)
            n += 1
            if n % 1000 == 0:
                f.flush()
        f.flush()


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--pairs-file", required=True)
    ap.add_argument("--out-dir", required=True)
    ap.add_argument("--worker-id", type=int, required=True)
    ap.add_argument("--workers", type=int, required=True)
    ap.add_argument("--lambda-rps", type=float, default=2.0)
    ap.add_argument("--size-bits", type=int, default=256)
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--warmup", type=float, default=5.0)
    ap.add_argument("--start-pairs", type=int, default=50)
    ap.add_argument("--step-pairs", type=int, default=50)
    ap.add_argument("--interval", type=float, default=15.0)
    ap.add_argument("--hold", type=float, default=120.0)
    ap.add_argument("--poisson", action="store_true",
                    help="true Poisson (exponential) inter-arrivals instead of uniform(0,2/lambda)")
    args = ap.parse_args()

    allp = json.loads(Path(args.pairs_file).read_text())
    mine = [p for i, p in enumerate(allp) if i % args.workers == args.worker_id]
    # local pair_id space (unique within this worker; aggregate keys on worker_id+pair_id)
    for j, p in enumerate(mine):
        p["pair_id"] = j
    out = Path(args.out_dir) / f"worker-{args.worker_id}"
    q: "queue.Queue" = queue.Queue(maxsize=100000)
    stop = threading.Event()
    wt = threading.Thread(target=writer, args=(out / "requests.csv", q, stop), daemon=True)
    wt.start()

    print(f"[rt w{args.worker_id}] {len(mine)} pairs; warmup {args.warmup}s", flush=True)
    time.sleep(args.warmup)
    threads = []
    active = 0
    target = min(args.start_pairs, len(mine))
    while True:
        while active < target:
            p = mine[active]
            t = threading.Thread(target=pair_loop,
                                 args=(p, args.lambda_rps, args.size_bits, args.timeout, q, stop,
                                       args.poisson),
                                 daemon=True)
            t.start(); threads.append(t); active += 1
        if target >= len(mine):
            break
        time.sleep(args.interval)
        target = min(target + args.step_pairs, len(mine))
    print(f"[rt w{args.worker_id}] hold {args.hold}s with {active} pairs active", flush=True)
    time.sleep(args.hold)
    stop.set()
    for t in threads:
        t.join(timeout=2.0)
    wt.join(timeout=10.0)
    print(f"[rt w{args.worker_id}] done → {out/'requests.csv'}", flush=True)


if __name__ == "__main__":
    main()
