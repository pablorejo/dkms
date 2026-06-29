#!/usr/bin/env python3
"""Minimal ETSI-014 mTLS smoke test against a running deployment (plan.json).

Two modes, auto-selected:

* **Static SAEs** (``plan["sae_assign"]`` non-empty, i.e. ``--pairs 0`` deploys):
  for each SAE it opens an mTLS connection to its home DKMS and issues N
  ``enc_keys`` requests for keys shared with its slave SAE.

* **Round-trip pairs** (``--pairs P`` deploys leave ``sae_assign`` empty and emit
  ``roundtrip_pairs.json``): smoke a small SAMPLE of the very pairs the saturation
  ramp will later hammer — a full ``enc_keys`` (master) → ``dec_keys`` (slave) →
  byte-match round-trip. This makes the smoke a real correctness gate in RT mode
  (the old code read the empty ``sae_assign`` and always reported 0/0).

Reports per-target status counts and an overall PASS/FAIL. Stdlib only
(http.client+ssl), so it runs on the cluster's system python without extra deps.
"""
from __future__ import annotations

import argparse
import http.client
import json
import ssl
import sys
from collections import Counter
from pathlib import Path
from urllib.parse import urlparse


def _ctx(certfile: str, keyfile: str | None = None) -> ssl.SSLContext:
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.load_cert_chain(certfile=certfile, keyfile=keyfile)
    return ctx


def _post(url: str, ctx: ssl.SSLContext, body: str, timeout: float):
    u = urlparse(url)
    conn = http.client.HTTPSConnection(u.hostname, u.port, context=ctx, timeout=timeout)
    try:
        path = u.path + (("?" + u.query) if u.query else "")
        conn.request("POST", path, body=body, headers={"Content-Type": "application/json"})
        resp = conn.getresponse()
        data = resp.read(4096)
        return resp.status, data
    finally:
        conn.close()


def smoke_static(plan: dict, reqs: int, size: int, timeout: float) -> int:
    total = Counter()
    sample_err = None
    per = []
    for a in plan["sae_assign"]:
        c = Counter()
        ctx = _ctx(a["cert"], a["key"])
        body = json.dumps({"number": 1, "size": size})
        url = a["url"].rstrip("/") + f"/api/v1/keys/{a['slave_sae']}/enc_keys"
        for _ in range(reqs):
            try:
                status, data = _post(url, ctx, body, timeout)
                c[status] += 1
                total[status] += 1
                if status != 200 and sample_err is None:
                    sample_err = f"{a['sae_id']}→{a['slave_sae']} HTTP {status}: {data[:200]}"
            except Exception as e:  # noqa: BLE001
                c["ERR"] += 1
                total["ERR"] += 1
                if sample_err is None:
                    sample_err = f"{a['sae_id']}→{a['slave_sae']} EXC: {type(e).__name__}: {e}"
        per.append((a["sae_id"], a["slave_sae"], dict(c)))

    print("=== smoke results (static enc_keys) ===")
    for sid, slave, c in per:
        print(f"  {sid:>12} → {slave:<12} {c}")
    print(f"=== totals: {dict(total)} ===")
    if sample_err:
        print(f"first non-200: {sample_err}")
    ok = total.get(200, 0)
    n = sum(total.values())
    print(f"PASS rate: {ok}/{n}")
    return 0 if ok == n and n > 0 else 1


def smoke_pairs(pairs: list[dict], k: int, reqs: int, size: int, timeout: float) -> int:
    """Full enc→dec→match round-trip on the first ``k`` pairs, ``reqs`` each."""
    enc_status = Counter()
    dec_status = Counter()
    matches = 0
    attempts = 0
    sample_err = None
    sample = pairs[:k]
    print(f"=== smoke results (round-trip on {len(sample)}/{len(pairs)} pairs) ===")
    for p in sample:
        cm = _ctx(p["master_pem"])
        cs = _ctx(p["slave_pem"])
        enc_body = json.dumps({"number": 1, "size": size})
        c = Counter()
        for _ in range(reqs):
            attempts += 1
            try:
                st, data = _post(p["enc_url"], cm, enc_body, timeout)
                enc_status[st] += 1
                if st != 200:
                    c[f"enc{st}"] += 1
                    if sample_err is None:
                        sample_err = f"{p['master_sae']}→{p['slave_sae']} enc HTTP {st}: {data[:160]}"
                    continue
                k_enc = json.loads(data)["keys"][0]
                key_id, key_master = k_enc["key_ID"], k_enc["key"]
                dec_body = json.dumps({"key_IDs": [{"key_ID": key_id}]})
                st2, data2 = _post(p["dec_url"], cs, dec_body, timeout)
                dec_status[st2] += 1
                if st2 != 200:
                    c[f"dec{st2}"] += 1
                    if sample_err is None:
                        sample_err = f"{p['master_sae']}→{p['slave_sae']} dec HTTP {st2}: {data2[:160]}"
                    continue
                key_slave = json.loads(data2)["keys"][0]["key"]
                if key_slave == key_master:
                    matches += 1
                    c["match"] += 1
                else:
                    c["mismatch"] += 1
                    if sample_err is None:
                        sample_err = f"{p['master_sae']}→{p['slave_sae']} BYTE MISMATCH"
            except Exception as e:  # noqa: BLE001
                c["ERR"] += 1
                if sample_err is None:
                    sample_err = f"{p['master_sae']}→{p['slave_sae']} EXC: {type(e).__name__}: {e}"
        print(f"  pair {p.get('pair_id','?'):>4} {p['master_sae']:>12} → {p['slave_sae']:<12} {dict(c)}")
    print(f"=== enc: {dict(enc_status)}  dec: {dict(dec_status)}  matches: {matches}/{attempts} ===")
    if sample_err:
        print(f"first problem: {sample_err}")
    # PASS = every attempt produced a byte-matching round-trip.
    print(f"PASS rate: {matches}/{attempts}")
    return 0 if matches == attempts and attempts > 0 else 1


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan", required=True)
    ap.add_argument("--reqs", type=int, default=5, help="requests per SAE / pair")
    ap.add_argument("--size-bits", type=int, default=None, help="defaults to plan key_bits")
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--pairs-file", default=None,
                    help="round-trip pairs JSON (default: <plan dir>/roundtrip_pairs.json)")
    ap.add_argument("--smoke-pairs", type=int, default=10, help="how many pairs to sample in RT mode")
    args = ap.parse_args()

    plan_path = Path(args.plan)
    plan = json.loads(plan_path.read_text())
    size = args.size_bits or plan["meta"]["key_bits"]

    if plan.get("sae_assign"):
        sys.exit(smoke_static(plan, args.reqs, size, args.timeout))

    # RT mode: smoke the round-trip pairs the saturation ramp will use.
    pf = Path(args.pairs_file) if args.pairs_file else (plan_path.parent / "roundtrip_pairs.json")
    if not pf.exists():
        print(f"=== smoke: empty sae_assign and no pairs file at {pf} → nothing to test ===")
        print("PASS rate: 0/0")
        sys.exit(1)
    pairs = json.loads(pf.read_text())
    sys.exit(smoke_pairs(pairs, args.smoke_pairs, args.reqs, size, args.timeout))


if __name__ == "__main__":
    main()
