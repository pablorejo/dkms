#!/usr/bin/env python3
"""Launch (or stop) a generated DKMS deployment from a ``plan.json``.

Phase order with readiness gates:  0 SDN  →  1 quditto+QKC  →  2 ORR  →  3 DKMS.

Single-node (loopback): launches every process locally. Multinode: pass
``--only-host <ip>`` so each Slurm rank starts just the processes whose
``host_ip`` matches its node (the SDN + cross-host readiness are handled by the
sbatch driver, which gates phases across all ranks).

Processes are detached (own process group) and survive this script; their PIDs
are written to ``<logs>/pids.json`` for ``--stop``.
"""
from __future__ import annotations

import argparse
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from pathlib import Path


def tcp_up(host: str, port: int, timeout: float = 1.0) -> bool:
    try:
        with socket.create_connection((host, port), timeout=timeout):
            return True
    except OSError:
        return False


def http_up(url: str, timeout: float = 1.0) -> bool:
    try:
        urllib.request.urlopen(url, timeout=timeout)
        return True
    except urllib.error.HTTPError:
        return True  # any HTTP response means the server is up
    except Exception:
        return False


def wait_ready(proc: dict, logf: Path, timeout: float = 40.0) -> bool:
    r = proc.get("ready")
    if not r:
        return True
    deadline = time.time() + timeout
    while time.time() < deadline:
        if r["type"] == "tcp" and tcp_up(r["host"], r["port"]):
            return True
        if r["type"] == "http" and http_up(r["url"]):
            return True
        if logf.exists() and logf.stat().st_size > 0:
            tail = logf.read_text(errors="replace").splitlines()[-3:]
            for line in tail:
                if "panicked" in line or "Error:" in line:
                    return False
        time.sleep(0.25)
    return False


def pids_path(args: argparse.Namespace) -> Path:
    return Path(args.pids_file) if args.pids_file else Path(args.logs) / "pids.json"


def start(args: argparse.Namespace) -> int:
    plan = json.loads(Path(args.plan).read_text())
    logs = Path(args.logs)
    logs.mkdir(parents=True, exist_ok=True)
    only = args.only_host
    procs = [p for p in plan["procs"] if (only is None or p["host_ip"] == only)]
    if args.phase is not None:
        procs = [p for p in procs if p["phase"] == args.phase]
    phases = sorted({p["phase"] for p in procs})
    pf = pids_path(args)
    pids: dict[str, int] = json.loads(pf.read_text()) if pf.exists() else {}

    for ph in phases:
        group = [p for p in procs if p["phase"] == ph]
        started = []
        for p in group:
            logf = logs / f"{p['name']}.log"
            env = dict(os.environ)
            env.update(p.get("env", {}))
            env.setdefault("RUST_LOG", args.rust_log)
            fh = open(logf, "wb")
            proc = subprocess.Popen(p["cmd"], stdout=fh, stderr=subprocess.STDOUT,
                                    env=env, start_new_session=True)
            pids[p["name"]] = proc.pid
            started.append((p, logf))
        # gate on readiness for this phase
        ok = True
        for p, logf in started:
            if not wait_ready(p, logf, timeout=args.timeout):
                print(f"[launch] ✗ {p['name']} not ready (phase {ph}); tail:")
                if logf.exists():
                    for line in logf.read_text(errors="replace").splitlines()[-12:]:
                        print("    " + line)
                ok = False
        print(f"[launch] phase {ph}: {len(started)} proc(s) "
              f"{'ready' if ok else 'INCOMPLETE'}")
        if not ok and not args.keep_going:
            pf.write_text(json.dumps(pids, indent=2))
            return 1

    pf.write_text(json.dumps(pids, indent=2))
    print(f"[launch] up: {len(pids)} process(es). pids → {pf}")
    return 0


def stop(args: argparse.Namespace) -> int:
    pf = pids_path(args)
    if not pf.exists():
        print(f"[launch] no pids file at {pf}")
        return 0
    pids = json.loads(pf.read_text())
    killed = 0
    for name, pid in pids.items():
        try:
            os.killpg(os.getpgid(pid), signal.SIGTERM)
            killed += 1
        except (ProcessLookupError, PermissionError):
            try:
                os.kill(pid, signal.SIGTERM)
                killed += 1
            except ProcessLookupError:
                pass
    print(f"[launch] sent SIGTERM to {killed}/{len(pids)} processes")
    return 0


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--plan", help="path to plan.json")
    ap.add_argument("--logs", required=True, help="dir for per-process logs + pids.json")
    ap.add_argument("--only-host", default=None, help="start only procs with this host_ip (multinode)")
    ap.add_argument("--phase", type=int, default=None, help="start only this phase (multinode barrier-driven)")
    ap.add_argument("--pids-file", default=None, help="pids file path (default <logs>/pids.json)")
    ap.add_argument("--rust-log", default="info,tonic=warn,h2=warn")
    ap.add_argument("--timeout", type=float, default=40.0, help="per-process readiness timeout (s)")
    ap.add_argument("--keep-going", action="store_true", help="don't abort a phase on a not-ready proc")
    ap.add_argument("--stop", action="store_true", help="stop a running deployment (reads pids.json)")
    args = ap.parse_args()
    if args.stop:
        sys.exit(stop(args))
    if not args.plan:
        ap.error("--plan required to start")
    sys.exit(start(args))


if __name__ == "__main__":
    main()
