"""
Local parallel SAE provisioning + traffic runner.

Replaces the `dkms-loadtest:v1` pod with an in-process orchestrator that:
  1. Provisions a ramp of SAEs in parallel against /orch/admin/saes +
     /orch/admin/saes/<id>/issue (concurrency configurable).
  2. As each SAE comes alive, spawns a worker thread that issues ETSI 014
     POST /enc_keys + POST /dec_keys against the runtime ingress at the
     requested λ_sae req/s. Each worker alternates methods using the
     ``--method-mix`` ratio; key_IDs from successful enc_keys are pushed to
     a shared broker so peer SAEs can issue real dec_keys with them.
  3. Records every request to <output_dir>/loadtest_requests.csv with a
     ``method`` column (``enc`` or ``dec``).
  4. Records each provisioning event to <output_dir>/loadtest_sae_timeline.csv.

Usage:
  python -m tests.cli.parallel_loadtest \\
      --sim-id 91 --orch-url http://127.0.0.1:18080 \\
      --start 100 --end 10100 --step 500 --interval 15 --lambda 10 \\
      --concurrency 20 --warmup 10 --output-dir tests/results/xxx \\
      --runtime-url https://dkms2.example.com/api/sim/91 --method-mix 0.5
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


# ----------- Key-ID broker (enc -> dec hand-off) -----------

class KeyIdBroker:
    """Cross-worker, thread-safe FIFO of (key_id, key_bytes_b64) by (master, slave).

    2026-05-24 iter-001 F2.0: el broker ahora guarda los BYTES de la key
    además del key_ID. Cuando el slave llama dec_keys con ese key_ID,
    compara los bytes recibidos con los originales del master — si no
    coinciden, es key_mismatch (bug grave del protocolo ETSI 014).
    """

    __slots__ = ("_lock", "_q", "_cap")

    def __init__(self, per_pair_cap: int = 256) -> None:
        self._lock = threading.Lock()
        self._q: dict[tuple[str, str], list[tuple[str, str]]] = {}
        self._cap = max(1, int(per_pair_cap))

    def push(self, master: str, slave: str, key_id: str, key_b64: str) -> None:
        if not key_id:
            return
        k = (master, slave)
        with self._lock:
            lst = self._q.setdefault(k, [])
            lst.append((key_id, key_b64))
            if len(lst) > self._cap:
                del lst[: len(lst) - self._cap]

    def pop(self, master: str, slave: str) -> tuple[str, str] | None:
        k = (master, slave)
        with self._lock:
            lst = self._q.get(k)
            if not lst:
                return None
            return lst.pop(0)

    def depth(self) -> int:
        with self._lock:
            return sum(len(v) for v in self._q.values())


# ----------- Traffic -----------

# Each SAE worker hits its DKMS via the runtime ingress over HTTPS+mTLS.
# We alternate enc_keys (master role) and dec_keys (slave role using a
# key_ID supplied by a peer's earlier enc_keys). When method=dec and no
# key_ID is available yet we fall back to enc so the worker keeps emitting
# traffic. Each request is labelled with the ``method`` column in CSV.

def _sae_traffic_worker(sae_info, peer_pool, lambda_req_s, key_size_bits,
                        request_timeout_s, runtime_url, runtime_verify,
                        rows_writer, rows_lock, stop_event, session,
                        broker, method_mix):
    rng = random.Random()
    bundle = sae_info["bundle"]
    cert_pem = bundle["certificate_pem"]
    key_pem = bundle["private_key_pem"]
    ca_pem = bundle["ca_chain_pem"]
    sae_id = sae_info["sae_id"]
    base = Path(f"/tmp/par-saes-{sae_info.get('test_id','x')}")
    base.mkdir(exist_ok=True, parents=True)
    cert_path = base / f"{sae_id}.crt"
    key_path = base / f"{sae_id}.key"
    ca_path = base / f"{sae_id}.ca"
    cert_path.write_text(cert_pem)
    key_path.write_text(key_pem)
    ca_path.write_text(ca_pem)
    cert_tuple = (str(cert_path), str(key_path))
    verify = str(ca_path) if runtime_verify else False

    def _write_row(method, peer, t_start, t_end, status_code,
                   n_keys_received, error, fallback=False, key_match=""):
        with rows_lock:
            rows_writer.writerow({
                "architecture": "qkd",
                "test_id": sae_info.get("test_id", ""),
                "sae_id": sae_id,
                "slave_sae_id": peer,
                "method": method,
                "fallback": "true" if fallback else "false",
                "request_index": rng.randrange(10**9),
                "emitted_at_epoch": t_start,
                "responded_at_epoch": t_end,
                "elapsed_seconds": t_end - t_start,
                "n_keys_requested": 1,
                "n_keys_received": n_keys_received,
                "status_code": status_code,
                "success": str(status_code == 200).lower(),
                "error": error,
                "key_match": key_match,
            })

    # Per-SAE base URL: each SAE must hit ITS OWN DKMS's runtime ingress,
    # because nginx routes /api/sim/<sim>/dkms/<HOST_id>/... → that DKMS
    # service. The service is named `dkms-{host_id}` (pods.py:437), so the
    # ingress path uses host_id, NOT the dkms BD primary key.
    own_host = sae_info.get("host_id") or sae_info.get("dkms_id")
    base = f"{runtime_url}/dkms/{own_host}"
    while not stop_event.is_set():
        wait = rng.expovariate(max(lambda_req_s, 0.01))
        if stop_event.wait(timeout=wait):
            break
        peer = rng.choice(peer_pool)
        if peer == sae_id:
            continue

        want_dec = rng.random() < method_mix
        method = "dec" if want_dec else "enc"
        popped = broker.pop(peer, sae_id) if want_dec else None
        used_key_id = popped[0] if popped else None
        expected_key_b64 = popped[1] if popped else None
        fallback = False
        if want_dec and used_key_id is None:
            # No key_ID yet from this peer — emit enc instead so the
            # worker keeps generating traffic. Marked as fallback so the
            # analyzer can keep the method mix honest.
            method = "enc"
            fallback = True

        if method == "enc":
            url = f"{base}/api/v1/keys/{peer}/enc_keys"
            body = {"number": 1, "size": int(key_size_bits)}
            t_start = time.time()
            try:
                r = session.post(url, json=body, cert=cert_tuple, verify=verify,
                                 timeout=request_timeout_s)
                status_code = r.status_code
                n_keys_received = 0
                if status_code == 200:
                    try:
                        payload = r.json()
                        keys = payload.get("keys", []) or []
                        n_keys_received = len(keys)
                        for k in keys:
                            kid = k.get("key_ID") or k.get("key_id")
                            kb = k.get("key") or ""
                            if kid:
                                # `me` is master here; peer is slave.
                                broker.push(sae_id, peer, kid, kb)
                    except Exception:
                        pass
                error = "" if status_code == 200 else (r.text[:200] if r.text else "")
            except requests.exceptions.RequestException as exc:
                status_code = 0
                n_keys_received = 0
                error = str(exc)[:200]
            t_end = time.time()
            _write_row("enc", peer, t_start, t_end, status_code,
                       n_keys_received, error, fallback=fallback)
        else:
            url = f"{base}/api/v1/keys/{peer}/dec_keys"
            body = {"key_IDs": [{"key_ID": used_key_id}]}
            t_start = time.time()
            key_match = ""
            try:
                r = session.post(url, json=body, cert=cert_tuple, verify=verify,
                                 timeout=request_timeout_s)
                status_code = r.status_code
                n_keys_received = 0
                if status_code == 200:
                    try:
                        payload = r.json()
                        keys = payload.get("keys", []) or []
                        n_keys_received = len(keys)
                        # F2.0: comparar key_bytes recibidos con los del
                        # master (originados en el push del broker).
                        if expected_key_b64 and keys:
                            received_b64 = keys[0].get("key") or ""
                            key_match = "true" if received_b64 == expected_key_b64 else "false"
                    except Exception:
                        pass
                error = "" if status_code == 200 else (r.text[:200] if r.text else "")
            except requests.exceptions.RequestException as exc:
                status_code = 0
                n_keys_received = 0
                error = str(exc)[:200]
            t_end = time.time()
            _write_row("dec", peer, t_start, t_end, status_code,
                       n_keys_received, error, key_match=key_match)


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
    # list_dkms[i].id is the DKMS BD primary key (used for SAE binding).
    # list_dkms[i].id_host is the K8s Service / Ingress path suffix
    # (orchestrator/pods.py:437  runtime_service_name = f"dkms-{local_host_id}").
    # We need BOTH: dkms_id to register the SAE, host_id to build the URL.
    dkms_list = [d["id"] for d in sim.get("list_dkms", [])]
    dkms_to_host = {d["id"]: d["id_host"] for d in sim.get("list_dkms", [])}
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
        "architecture", "test_id", "sae_id", "slave_sae_id", "method",
        "fallback", "request_index",
        "emitted_at_epoch", "responded_at_epoch", "elapsed_seconds",
        "n_keys_requested", "n_keys_received", "status_code", "success", "error",
        "key_match",
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
    broker = KeyIdBroker(per_pair_cap=int(getattr(args, "broker_cap", 256)))
    method_mix = float(getattr(args, "method_mix", 0.5))
    method_mix = max(0.0, min(1.0, method_mix))

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
    use_bulk = getattr(args, "use_bulk", True)
    for step_i, target in enumerate(targets):
        n_new = target - next_idx
        if n_new <= 0:
            continue
        print(f"[par-loadtest] step {step_i+1}/{len(targets)} target={target} provisioning {n_new} SAEs (bulk={use_bulk})")
        t_step = time.time()
        new_workers: list[dict[str, Any]] = []
        if use_bulk:
            # 2026-05-20: bulk-provision the whole step in a single POST
            # to /orch/admin/saes/bulk. The orchestator inserts N SAE
            # rows in 1 transaction + does 1 sync to SDN /sae-bulk (one
            # write_mux lock + one topology clone for the whole batch).
            # Drastically reduces the SDN bottleneck observed under
            # parallel single-provision (it took ~10s for the SDN to
            # process ~100 SAEs sequentially).
            bulk_items = []
            for i in range(n_new):
                idx = next_idx + i
                dkms_id = dkms_list[idx % len(dkms_list)]
                bulk_items.append({
                    "simulation_id": args.sim_id,
                    "dkms_id": dkms_id,
                    "sae_id": f"par-{test_id}-{idx:05d}",
                    "description": "parallel-loadtest-bulk",
                })
            try:
                r = sess_provision.post(
                    f"{args.orch_url}/orch/admin/saes/bulk",
                    params={"issue_certs": "true", "days_valid": "1"},
                    headers={"X-User-Id": str(uid)},
                    json=bulk_items,
                    timeout=180,
                )
                if r.status_code == 200:
                    res = r.json()
                    created = int(res.get("created", 0))
                    sdn_ok = int(res.get("sdn_ok", 0))
                    failed_step = int(res.get("failed", 0))
                    bundles_by_sae = {
                        b["sae_id"]: b for b in res.get("bundles", [])
                    }
                    # Record each successfully created SAE in timeline +
                    # build worker entries with cert bundle for traffic.
                    with sae_lock:
                        for i in range(n_new):
                            sae_id = bulk_items[i]["sae_id"]
                            b = bundles_by_sae.get(sae_id)
                            if not b:
                                continue
                            idx = next_idx + i
                            sae_writer.writerow({
                                "sae_index": idx,
                                "sae_id": sae_id,
                                "dkms_id": bulk_items[i]["dkms_id"],
                                "src_ingress_id": bulk_items[i]["dkms_id"],
                                "provisioned_at_epoch": time.time(),
                                "provisioned_at_iso": time.strftime(
                                    "%Y-%m-%dT%H:%M:%S+00:00", time.gmtime()),
                            })
                            worker = {
                                "sae_id": sae_id,
                                "dkms_id": bulk_items[i]["dkms_id"],
                                "ok": True,
                                "bundle": {
                                    "certificate_pem": b["certificate_pem"],
                                    "private_key_pem": b["private_key_pem"],
                                    "ca_chain_pem": b["ca_chain_pem"],
                                },
                                "test_id": test_id,
                                "host_id": dkms_to_host.get(bulk_items[i]["dkms_id"]),
                                "idx": idx,
                            }
                            new_workers.append(worker)
                            provisioned.append(worker)
                    issued = len(bundles_by_sae)
                    failed += max(0, n_new - issued)
                    print(f"[par-loadtest]   bulk step: created={created} issued={issued} sdn_ok={sdn_ok} failed={max(0, n_new - issued)}")
                else:
                    failed += n_new
                    print(f"[par-loadtest]   bulk step FAILED status={r.status_code} body={r.text[:200]}")
            except Exception as exc:
                failed += n_new
                print(f"[par-loadtest]   bulk step EXCEPTION: {str(exc)[:200]}")
        else:
            # Legacy path: parallel single provisions (kept for comparison)
            futs = []
            for i in range(n_new):
                idx = next_idx + i
                dkms_id = dkms_list[idx % len(dkms_list)]
                futs.append(provision_pool.submit(
                    _provision_one, sess_provision, args.orch_url, args.sim_id,
                    dkms_id, uid, idx, test_id, sae_writer, sae_lock,
                ))
            for f in as_completed(futs):
                res = f.result()
                if res["ok"]:
                    res["test_id"] = test_id
                    # Resolve host_id for URL construction
                    res["host_id"] = dkms_to_host.get(res["dkms_id"])
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
                    broker, method_mix,
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

    # F2.0: count key_mismatch from CSV. dec rows where key_match=="false".
    key_mismatch_count = 0
    key_match_count = 0
    try:
        with open(req_csv) as f:
            for row in csv.DictReader(f):
                if row.get("method") == "dec":
                    km = row.get("key_match", "")
                    if km == "false":
                        key_mismatch_count += 1
                    elif km == "true":
                        key_match_count += 1
    except Exception:
        pass

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
        "method_mix": method_mix,
        "broker_depth_end": broker.depth(),
        "runtime_url": runtime_url,
        "key_mismatch_count": key_mismatch_count,
        "key_match_count": key_match_count,
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
    ap.add_argument("--password", default="config_user",
                    help="authz password (default 'config_user', matching dkms_topo.DEFAULT_PASSWORD)")
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
    ap.add_argument("--no-bulk", dest="use_bulk", action="store_false",
                    help="disable bulk provisioning (legacy parallel-singular path)")
    ap.set_defaults(use_bulk=True)
    ap.add_argument("--runtime-url", default=None,
                    help="HTTPS base URL of runtime ingress (e.g. https://dkms2.example.com/api/sim/91)")
    ap.add_argument("--runtime-verify", action="store_true",
                    help="Verify runtime TLS chain (default: skip verify)")
    ap.add_argument("--method-mix", type=float, default=0.5,
                    help=("fraction of attempts that prefer dec_keys (0.0=all "
                          "enc, 0.5=50/50, 1.0=all dec). dec_keys falls back "
                          "to enc when no key_ID is available yet (default 0.5)"))
    ap.add_argument("--broker-cap", type=int, default=256,
                    help="max key_IDs queued per (master,slave) pair (default 256)")
    ap.add_argument("--output-dir", required=True)
    args = ap.parse_args()
    run_loadtest(args)


if __name__ == "__main__":
    main()
