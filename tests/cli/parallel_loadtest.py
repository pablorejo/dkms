"""
Local parallel SAE provisioning + traffic runner.

Replaces the `dkms-loadtest:v1` pod with an in-process orchestrator that:
  1. Provisions a ramp of SAEs in parallel against /orch/admin/saes +
     /orch/admin/saes/<id>/issue (concurrency configurable).
  2. As each SAE comes alive, spawns a worker thread that sends GET
     /api/v1/keys/<peer>/enc_keys at the requested λ_sae req/s.
  3. Records every request to <output_dir>/loadtest_requests.csv with the
     same schema the loadtest pod emits.
  4. Records each provisioning event to <output_dir>/loadtest_sae_timeline.csv.

Usage:
  python -m tests.cli.parallel_loadtest \\
      --sim-id 91 --orch-url http://127.0.0.1:18080 \\
      --start 100 --end 10100 --step 500 --interval 15 --lambda 10 \\
      --concurrency 20 --warmup 10 --output-dir tests/results/xxx
"""
from __future__ import annotations

import argparse
import base64
import csv
import json
import math
import random
import ssl
import threading
import time
import uuid
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path
from typing import Any

import requests
from requests.adapters import HTTPAdapter
import urllib3
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)


def _login(authz_url: str, username: str, password: str) -> tuple[str, int]:
    r = requests.post(f"{authz_url}/login",
                      json={"username": username, "password": password}, timeout=15)
    r.raise_for_status()
    tok = r.json()["access_token"]
    p = tok.split(".")[1]
    p += "=" * (4 - len(p) % 4)
    uid = json.loads(base64.b64decode(p))["uid"]
    return tok, uid


def _sim_info(session: requests.Session, orch_url: str, sim_id: int, uid: int) -> dict[str, Any]:
    r = session.get(f"{orch_url}/orch/api/sim/{sim_id}",
                    headers={"X-User-Id": str(uid)}, timeout=15)
    r.raise_for_status()
    return r.json()


def _make_session(pool: int) -> requests.Session:
    s = requests.Session()
    a = HTTPAdapter(pool_connections=pool, pool_maxsize=pool, max_retries=0)
    s.mount("http://", a)
    s.mount("https://", a)
    return s


# ----------- Provision -----------

def _provision_one(session, orch_url, sim_id, dkms_id, uid, idx, test_id,
                   timeline_writer, timeline_lock):
    sae_id = f"par-{test_id}-{idx:05d}"
    t0 = time.time()
    try:
        r = session.post(f"{orch_url}/orch/admin/saes",
                         headers={"X-User-Id": str(uid)},
                         json={"simulation_id": sim_id, "dkms_id": dkms_id,
                               "sae_id": sae_id, "description": "parallel-loadtest"},
                         timeout=60)
        r.raise_for_status()
        r = session.post(f"{orch_url}/orch/admin/saes/{sae_id}/issue",
                         headers={"X-User-Id": str(uid)},
                         json={"simulation_id": sim_id, "days_valid": 1},
                         timeout=60)
        r.raise_for_status()
        bundle = r.json()
        with timeline_lock:
            timeline_writer.writerow({
                "sae_index": idx,
                "sae_id": sae_id,
                "dkms_id": dkms_id,
                "src_ingress_id": dkms_id,
                "provisioned_at_epoch": time.time(),
                "provisioned_at_iso": time.strftime("%Y-%m-%dT%H:%M:%S+00:00",
                                                   time.gmtime()),
            })
        return {"sae_id": sae_id, "dkms_id": dkms_id, "ok": True,
                "bundle": bundle, "t_total_s": time.time() - t0, "idx": idx}
    except Exception as exc:
        return {"sae_id": sae_id, "ok": False, "error": str(exc)[:300],
                "t_total_s": time.time() - t0, "idx": idx}


# ----------- Traffic -----------

# Each SAE worker hits its DKMS via cluster-internal URL (we go through
# port-forward to ingress, which is heavier). To keep things simple and
# the loadtest reproducible we use the runtime DNS over HTTPS via the
# orchestator gateway, falling back to skip when not reachable. For the
# purpose of measuring 429s the request bookkeeping is what matters.

def _sae_traffic_worker(sae_info, peer_pool, lambda_req_s, key_size_bits,
                        request_timeout_s, runtime_url, runtime_verify,
                        rows_writer, rows_lock, stop_event, session):
    rng = random.Random()
    interval = 1.0 / max(lambda_req_s, 0.01)
    bundle = sae_info["bundle"]
    cert_pem = bundle["certificate_pem"]
    key_pem = bundle["private_key_pem"]
    ca_pem = bundle["ca_chain_pem"]
    # write cert+key to /tmp for requests mTLS
    sae_id = sae_info["sae_id"]
    base = Path(f"/tmp/par-saes-{sae_info.get('test_id','x')}")
    base.mkdir(exist_ok=True, parents=True)
    cert_path = base / f"{sae_id}.crt"
    key_path = base / f"{sae_id}.key"
    ca_path = base / f"{sae_id}.ca"
    cert_path.write_text(cert_pem)
    key_path.write_text(key_pem)
    ca_path.write_text(ca_pem)
    while not stop_event.is_set():
        # Poisson-distributed inter-arrival
        wait = rng.expovariate(lambda_req_s)
        if stop_event.wait(timeout=wait):
            break
        peer = rng.choice(peer_pool)
        url = f"{runtime_url}/api/v1/keys/{peer}/enc_keys"
        params = {"size": str(key_size_bits)}
        t_start = time.time()
        try:
            r = session.get(url, params=params,
                            cert=(str(cert_path), str(key_path)),
                            verify=str(ca_path) if runtime_verify else False,
                            timeout=request_timeout_s)
            status_code = r.status_code
            try:
                body = r.json()
                n_keys_received = len(body.get("keys", []))
            except Exception:
                n_keys_received = 0
            error = ""
        except requests.exceptions.RequestException as exc:
            status_code = 0
            n_keys_received = 0
            error = str(exc)[:200]
        t_end = time.time()
        with rows_lock:
            rows_writer.writerow({
                "architecture": "qkd",
                "test_id": sae_info.get("test_id", ""),
                "sae_id": sae_id,
                "slave_sae_id": peer,
                "request_index": rng.randrange(10**9),  # not strictly monotonic
                "emitted_at_epoch": t_start,
                "responded_at_epoch": t_end,
                "elapsed_seconds": t_end - t_start,
                "n_keys_requested": 1,
                "n_keys_received": n_keys_received,
                "status_code": status_code,
                "success": str(status_code == 200).lower(),
                "error": error,
            })


# ----------- Ramp orchestration -----------

def run_loadtest(args) -> dict[str, Any]:
    output_dir = Path(args.output_dir)
    output_dir.mkdir(parents=True, exist_ok=True)
    data_dir = output_dir / "data"
    data_dir.mkdir(exist_ok=True)
    test_id = f"par-{int(time.time())}-{uuid.uuid4().hex[:6]}"
    print(f"[par-loadtest] test_id={test_id} output={output_dir}")

    sess_boot = _make_session(pool=8)
    tok, uid = _login(args.authz_url, args.username, args.password)
    print(f"[par-loadtest] logged in uid={uid}")
    sim = _sim_info(sess_boot, args.orch_url, args.sim_id, uid)
    dkms_list = [d["id"] for d in sim.get("list_dkms", [])]
    if not dkms_list:
        raise RuntimeError(f"sim {args.sim_id} has no DKMSs")
    print(f"[par-loadtest] sim_id={args.sim_id} dkms_count={len(dkms_list)}")

    # Resolve runtime ingress URL from sim metadata (must be reachable).
    runtime_url = args.runtime_url
    if runtime_url is None:
        # The runtime ingress for sim N is by convention at
        # https://<INGRESS_HOST>/api/sim/<id>/...
        # We rely on the user to pass --runtime-url; otherwise tests run
        # only the provision phase.
        print("[par-loadtest] WARN: no --runtime-url, traffic phase disabled")

    # CSV writers
    req_csv = data_dir / "loadtest_requests.csv"
    sae_csv = data_dir / "loadtest_sae_timeline.csv"
    req_fp = open(req_csv, "w", newline="")
    sae_fp = open(sae_csv, "w", newline="")
    req_writer = csv.DictWriter(req_fp, fieldnames=[
        "architecture", "test_id", "sae_id", "slave_sae_id", "request_index",
        "emitted_at_epoch", "responded_at_epoch", "elapsed_seconds",
        "n_keys_requested", "n_keys_received", "status_code", "success", "error",
    ])
    req_writer.writeheader()
    sae_writer = csv.DictWriter(sae_fp, fieldnames=[
        "sae_index", "sae_id", "dkms_id", "src_ingress_id",
        "provisioned_at_epoch", "provisioned_at_iso",
    ])
    sae_writer.writeheader()
    req_lock = threading.Lock()
    sae_lock = threading.Lock()

    sess_provision = _make_session(pool=max(args.concurrency, 20))
    sess_traffic = _make_session(pool=max(args.end_saes, 200))

    stop_event = threading.Event()
    provision_pool = ThreadPoolExecutor(max_workers=args.concurrency)
    traffic_pool = ThreadPoolExecutor(max_workers=args.end_saes + 10)

    provisioned: list[dict[str, Any]] = []
    failed: int = 0

    # Warmup
    if args.warmup > 0:
        print(f"[par-loadtest] warmup {args.warmup}s")
        time.sleep(args.warmup)

    # Ramp: steps of `step` SAEs, one per `interval` seconds, until end_saes.
    next_idx = 0
    t_ramp_start = time.time()
    targets = list(range(args.start_saes, args.end_saes + 1, args.step_saes))
    for step_i, target in enumerate(targets):
        n_new = target - next_idx
        if n_new <= 0:
            continue
        print(f"[par-loadtest] step {step_i+1}/{len(targets)} target={target} provisioning {n_new} SAEs")
        t_step = time.time()
        # Provision new SAEs in parallel
        futs = []
        for i in range(n_new):
            idx = next_idx + i
            dkms_id = dkms_list[idx % len(dkms_list)]
            futs.append(provision_pool.submit(
                _provision_one, sess_provision, args.orch_url, args.sim_id,
                dkms_id, uid, idx, test_id, sae_writer, sae_lock,
            ))
        new_workers: list[dict[str, Any]] = []
        for f in as_completed(futs):
            res = f.result()
            if res["ok"]:
                res["test_id"] = test_id
                new_workers.append(res)
                provisioned.append(res)
            else:
                failed += 1
        next_idx = target
        sae_fp.flush()
        # Spawn traffic workers for new SAEs (if runtime URL provided)
        if runtime_url is not None and new_workers:
            peer_pool = [w["sae_id"] for w in provisioned]  # pair against any provisioned SAE
            for w in new_workers:
                traffic_pool.submit(
                    _sae_traffic_worker, w, peer_pool, args.lambda_sae,
                    args.key_size_bits, args.request_timeout_s,
                    runtime_url, args.runtime_verify,
                    req_writer, req_lock, stop_event, sess_traffic,
                )
        elapsed = time.time() - t_step
        print(f"[par-loadtest]   step done: {len(new_workers)}/{n_new} OK, "
              f"failed={n_new - len(new_workers)}, took {elapsed:.1f}s "
              f"(running provisioned total: {len(provisioned)})")
        # Wait remainder of `interval` between steps
        sleep_remain = max(0.0, args.interval_seconds - elapsed)
        if step_i < len(targets) - 1 and sleep_remain > 0:
            time.sleep(sleep_remain)
        req_fp.flush()
    t_ramp_done = time.time()
    print(f"[par-loadtest] ramp complete: {len(provisioned)}/{next_idx} provisioned, "
          f"{failed} failed, took {t_ramp_done - t_ramp_start:.1f}s")

    # Hold traffic for hold_seconds at peak
    if args.hold_seconds > 0:
        print(f"[par-loadtest] holding at peak for {args.hold_seconds}s")
        time.sleep(args.hold_seconds)

    # Shutdown
    print("[par-loadtest] stopping traffic workers")
    stop_event.set()
    traffic_pool.shutdown(wait=True, cancel_futures=True)
    provision_pool.shutdown(wait=False, cancel_futures=True)
    req_fp.close()
    sae_fp.close()

    # Summary
    summary = {
        "test_id": test_id,
        "sim_id": args.sim_id,
        "provisioned": len(provisioned),
        "provision_failed": failed,
        "ramp_seconds": t_ramp_done - t_ramp_start,
        "ramp_targets": targets,
        "concurrency": args.concurrency,
        "lambda_sae": args.lambda_sae,
        "runtime_url": runtime_url,
    }
    (output_dir / "loadtest_summary.json").write_text(json.dumps(summary, indent=2))
    print(f"[par-loadtest] summary written → {output_dir}/loadtest_summary.json")
    return summary


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sim-id", type=int, required=True)
    ap.add_argument("--orch-url", default="http://127.0.0.1:18080")
    ap.add_argument("--authz-url", default="http://127.0.0.1:18081")
    ap.add_argument("--username", default="config_user")
    ap.add_argument("--password", default="config_password")
    ap.add_argument("--start-saes", type=int, default=100)
    ap.add_argument("--end-saes", type=int, default=10100)
    ap.add_argument("--step-saes", type=int, default=500)
    ap.add_argument("--interval-seconds", type=float, default=15.0)
    ap.add_argument("--warmup", type=float, default=10.0)
    ap.add_argument("--lambda-sae", type=float, default=10.0)
    ap.add_argument("--key-size-bits", type=int, default=256)
    ap.add_argument("--request-timeout-s", type=float, default=30.0)
    ap.add_argument("--concurrency", type=int, default=20)
    ap.add_argument("--hold-seconds", type=float, default=60.0)
    ap.add_argument("--runtime-url", default=None,
                    help="HTTPS base URL of runtime ingress (e.g. https://dkms2.example.com/api/sim/91)")
    ap.add_argument("--runtime-verify", action="store_true",
                    help="Verify runtime TLS chain (default: skip verify)")
    ap.add_argument("--output-dir", required=True)
    args = ap.parse_args()
    run_loadtest(args)


if __name__ == "__main__":
    main()
