"""``dkms-topo`` CLI entry point.

Generates DKMS topology payloads programmatically (ring/mesh/line/star/random),
ships them to the orchestator on EKS via its HTTP API, captures saturation
logs and (optionally) a SAE ramp, and compares observed rates against the
theoretical max-min model.

Implementation uses **``argparse`` (stdlib)** — see
``agent-dkms-topo-cli/.architecture.md`` "Historial de cambios
arquitectónicos" 2026-05-18 (iter 017) for the rationale.

Subcommands:

* ``ring -n N``
* ``mesh -n N -m M``
* ``line -n N``
* ``star -p PER_BRANCH_N -b BRANCHES``
* ``random -n N -d AVG_DEGREE [--seed S]``

Global flags (per subcommand):

* ``--name NAME``: simulation name (default auto-generated).
* ``--owner UID``: owner_uid for description seeding.
* ``--r0 FLOAT``: per-link R0 in keys/s (default ``2000.0``).
* ``--alpha FLOAT``: per-link alpha (default ``0.0``; orchestator
  applies 0.2 — R-011).
* ``--distance INT``: per-link distance in km (default ``5``).
* ``--buffer-enc-size INT``: per-peer ENC buffer capacity
  (default ``65536``).
* ``--sdn-endpoint STR``: ``ip:port`` or full ``http(s)://host:port``.
* ``--dry-run``: emit JSON to stdout, do NOT contact EKS.

The CLI is **read-only** w.r.t. the runtime (no Rust compilation, no
``kubectl apply``); it only consumes the orchestator HTTP API and
``kubectl logs|wait|exec`` via the helper modules.
"""

from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import Any

from tests.cli.analyze import DEFAULT_SAT_THRESHOLD, analyze_saturation
from tests.cli.loadtest_analyze import analyze_loadtest
from tests.cli.loadtest_runner import submit_and_wait_loadtest
from tests.cli.log_capture import tail_pods
from tests.cli.maxmin_theory import (
    DEFAULT_EFFECTIVE_ALPHA,
    all_pairs_commodities,
    topology_to_capacities,
    topology_to_graph,
    weighted_maxmin,
)
from tests.cli.orchestator_client import (
    AlreadyExistsError,
    OrchestatorClient,
    OrchestatorError,
)
from tests.cli.prom_parser import parse_metrics
from tests.cli.sat_parser import parse_generator_state
from tests.cli.sim_payload import build_sim_payload
from tests.cli.topology_builders import (
    DEFAULT_ALPHA,
    DEFAULT_BUFFER_SIZE,
    DEFAULT_DISTANCE_KM,
    DEFAULT_R0,
    build_barabasi_albert,
    build_bridge,
    build_er,
    build_line,
    build_mesh,
    build_rgg,
    build_ring,
    build_secoqc,
    build_star,
)

VERSION = "0.1.0"

DEFAULT_RESULTS_BASE = Path("tests/results")
DEFAULT_SATURATION_TIMEOUT_S = 600.0
DEFAULT_POLL_INTERVAL_S = 5.0
DEFAULT_POD_READY_TIMEOUT_S = 300.0
DEFAULT_POD_SELECTOR = "app=dkms"
DEFAULT_USERNAME = "config_user"
DEFAULT_PASSWORD = "config_user"


def _add_global_flags(p: argparse.ArgumentParser) -> None:
    p.add_argument(
        "--name",
        default=None,
        help="simulation name (default: auto from subcommand + parameters)",
    )
    p.add_argument(
        "--owner",
        type=int,
        default=None,
        metavar="UID",
        help="owner_uid for description seeding (X-User-Id header carries auth)",
    )
    p.add_argument(
        "--r0",
        type=float,
        default=DEFAULT_R0,
        help=f"per-link R0 in keys/s (default {DEFAULT_R0})",
    )
    p.add_argument(
        "--alpha",
        type=float,
        default=DEFAULT_ALPHA,
        help=(
            f"per-link alpha (default {DEFAULT_ALPHA}; orchestator applies 0.2 "
            "internally per R-011)"
        ),
    )
    p.add_argument(
        "--distance",
        type=int,
        default=DEFAULT_DISTANCE_KM,
        help=f"per-link distance in km (default {DEFAULT_DISTANCE_KM})",
    )
    p.add_argument(
        "--buffer-enc-size",
        type=int,
        default=DEFAULT_BUFFER_SIZE,
        help=f"per-peer ENC buffer capacity (default {DEFAULT_BUFFER_SIZE})",
    )
    p.add_argument(
        "--sdn-endpoint",
        default=None,
        help="orchestator SDN endpoint: 'ip:port' or full 'http(s)://host:port'",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help="emit JSON payload to stdout and exit; do NOT contact EKS",
    )
    p.add_argument(
        "--buffer-saturated",
        action="store_true",
        help=(
            "create sim, run it, tail DKMS logs, wait until all commodities "
            "reach enc >= sat_threshold * buffer, run analysis, stop sim, "
            "save results under --output-dir"
        ),
    )
    p.add_argument(
        "--output-dir",
        default=None,
        help=(
            "directory for log captures + analyses (default: "
            "tests/results/<utc_iso>-<topology_name>/)"
        ),
    )
    p.add_argument(
        "--saturation-timeout",
        type=float,
        default=DEFAULT_SATURATION_TIMEOUT_S,
        help=f"seconds to wait for all peers to saturate (default {int(DEFAULT_SATURATION_TIMEOUT_S)})",
    )
    p.add_argument(
        "--sat-threshold",
        type=float,
        default=DEFAULT_SAT_THRESHOLD,
        help=f"saturation threshold as fraction of buffer (default {DEFAULT_SAT_THRESHOLD})",
    )
    p.add_argument(
        "--no-plots",
        dest="write_plots",
        action="store_false",
        default=True,
        help=(
            "skip writing PNG plots under <output_dir>/plots/. CSVs in "
            "<output_dir>/data/ are still written (use --no-csv-export "
            "to skip those too). Default: plots enabled."
        ),
    )
    p.add_argument(
        "--no-csv-export",
        dest="write_csv",
        action="store_false",
        default=True,
        help=(
            "skip writing CSV exports under <output_dir>/data/. PNGs in "
            "<output_dir>/plots/ are still written. Default: CSVs enabled."
        ),
    )
    p.add_argument(
        "--effective-alpha",
        type=float,
        default=DEFAULT_EFFECTIVE_ALPHA,
        help=(
            f"alpha used for the THEORETICAL rate calculation (default "
            f"{DEFAULT_EFFECTIVE_ALPHA}; orchestator ignores alpha=0 and "
            "applies 0.2 internally — R-011)"
        ),
    )
    p.add_argument(
        "--authz-url",
        default="http://127.0.0.1:18081",
        help="authz port-forward URL (default http://127.0.0.1:18081)",
    )
    p.add_argument(
        "--orch-url",
        default="http://127.0.0.1:18080",
        help="orchestator port-forward URL (default http://127.0.0.1:18080)",
    )
    p.add_argument(
        "--username",
        default=DEFAULT_USERNAME,
        help=f"authz username (default {DEFAULT_USERNAME})",
    )
    p.add_argument(
        "--password",
        default=DEFAULT_PASSWORD,
        help="authz password (default config_user)",
    )
    p.add_argument(
        "--namespace",
        default=None,
        help="kubectl namespace for the sim (default sim-<sim_id>-ns)",
    )
    # ---- SAE ramp (OBJ-019) ---------------------------------------------
    p.add_argument(
        "--sae-test",
        action="store_true",
        help=(
            "after buffer-saturation, run a SAE ramp loadtest. With --no-fill, "
            "skip the saturation phase and ramp directly."
        ),
    )
    p.add_argument(
        "--no-fill",
        action="store_true",
        help=(
            "skip buffer-saturation; go straight to --sae-test on a freshly "
            "started sim"
        ),
    )
    p.add_argument(
        "--sae-start",
        type=int,
        default=5,
        help="starting number of SAEs in the ramp (default 5)",
    )
    p.add_argument(
        "--sae-end",
        type=int,
        default=50,
        help="ending number of SAEs in the ramp (default 50)",
    )
    p.add_argument(
        "--sae-step",
        type=int,
        default=5,
        help="SAEs added per step in the ramp (default 5)",
    )
    p.add_argument(
        "--time-step",
        type=float,
        default=15.0,
        help=(
            "seconds between ramp steps; maps to interval_seconds "
            "(default 15.0)"
        ),
    )
    p.add_argument(
        "--lambda-sae",
        type=float,
        default=0.5,
        help=(
            "per-SAE request rate λ in req/s; maps to per_sae_lambda "
            "(default 0.5)"
        ),
    )
    p.add_argument(
        "--sae-warmup",
        type=float,
        default=30.0,
        help="warmup_seconds before the ramp starts (default 30.0)",
    )
    p.add_argument(
        "--sae-key-size",
        type=int,
        default=256,
        help="key_size_bits requested by each SAE (default 256)",
    )
    p.add_argument(
        "--sae-request-timeout",
        type=int,
        default=60,
        help="per-request timeout in seconds (default 60)",
    )
    p.add_argument(
        "--loadtest-duration",
        type=float,
        default=None,
        help=(
            "max wall-clock for the loadtest (default: derived from ramp "
            "params plus a 60s safety margin)"
        ),
    )
    p.add_argument(
        "--force",
        action="store_true",
        help=(
            "skip the interactive confirmation when deleting pre-existing "
            "sims with the same name (default: prompt before deleting)"
        ),
    )
    p.add_argument(
        "--yes",
        dest="assume_yes",
        action="store_true",
        help="alias for --force; assume 'yes' to all prompts",
    )
    p.add_argument(
        "--pod-ready-timeout",
        type=float,
        default=DEFAULT_POD_READY_TIMEOUT_S,
        help=(
            f"seconds to wait for DKMS pods to reach Ready before tailing "
            f"logs (default {int(DEFAULT_POD_READY_TIMEOUT_S)}). Avoids "
            "'kubectl logs -f' failing with PodInitializing."
        ),
    )
    p.add_argument(
        "--pod-selector",
        default=DEFAULT_POD_SELECTOR,
        help=f"k8s label selector for DKMS pods (default {DEFAULT_POD_SELECTOR})",
    )
    p.add_argument(
        "--node-id-offset",
        type=int,
        default=0,
        help=(
            "shift every node_id by this offset (default 0). The orchestator "
            "derives local_qkc_id = 100000 + node_id; if another simulation "
            "in the cluster already uses ids 1..N, set this to e.g. 100 to "
            "produce ids 101..N+100 and avoid the kme PK collision."
        ),
    )


def _auto_name(subcmd: str, args: argparse.Namespace) -> str:
    """Pick a deterministic simulation name when --name is omitted."""
    if subcmd == "ring":
        return f"ring-n{args.n}"
    if subcmd == "line":
        return f"line-n{args.n}"
    if subcmd == "mesh":
        return f"mesh-{args.n}x{args.m}"
    if subcmd == "star":
        return f"star-b{args.b}-p{args.p}"
    if subcmd == "er":
        seed = args.seed if args.seed is not None else "rnd"
        return f"er-n{args.n}-k{args.k}-s{seed}"
    if subcmd == "ba":
        seed = args.seed if args.seed is not None else "rnd"
        return f"ba-n{args.n}-k{args.k}-s{seed}"
    if subcmd == "rgg":
        seed = args.seed if args.seed is not None else "rnd"
        return f"rgg-n{args.n}-r{args.max_distance_km}-k{args.k}-s{seed}"
    if subcmd == "secoqc":
        seed = args.seed if args.seed is not None else "rnd"
        return f"secoqc-n{args.n}-k{args.k}-s{seed}"
    if subcmd == "bridge":
        return f"bridge-c{args.cluster_count}-n{args.cluster_n}"
    return subcmd


def _build_topology(subcmd: str, args: argparse.Namespace) -> dict[str, Any]:
    if subcmd == "ring":
        return build_ring(args.n)
    if subcmd == "line":
        return build_line(args.n)
    if subcmd == "mesh":
        return build_mesh(args.n, args.m)
    if subcmd == "star":
        return build_star(per_branch_n=args.p, branches=args.b)
    if subcmd == "er":
        return build_er(n=args.n, avg_degree=args.k, seed=args.seed)
    if subcmd == "ba":
        return build_barabasi_albert(n=args.n, avg_degree=args.k, seed=args.seed)
    if subcmd == "rgg":
        return build_rgg(
            n=args.n,
            max_distance_km=args.max_distance_km,
            avg_degree=args.k,
            seed=args.seed,
        )
    if subcmd == "secoqc":
        return build_secoqc(n=args.n, avg_degree=args.k, seed=args.seed)
    if subcmd == "bridge":
        return build_bridge(
            cluster_n=args.cluster_n, cluster_count=args.cluster_count
        )
    raise ValueError(f"unknown subcommand {subcmd!r}")


def _build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="dkms-topo",
        description=(
            "Generate DKMS topology payloads and run them on EKS dkms1, "
            "comparing observed saturation against the theoretical max-min model."
        ),
    )
    parser.add_argument(
        "--version", action="version", version=f"dkms-topo {VERSION}"
    )
    sub = parser.add_subparsers(dest="topology", metavar="TOPOLOGY")
    sub.required = True

    p_ring = sub.add_parser("ring", help="N-node ring topology (n >= 3)")
    p_ring.add_argument("-n", type=int, required=True, help="number of nodes (>=3)")
    _add_global_flags(p_ring)

    p_line = sub.add_parser("line", help="N-node line/path topology (n >= 2)")
    p_line.add_argument("-n", type=int, required=True, help="number of nodes (>=2)")
    _add_global_flags(p_line)

    p_mesh = sub.add_parser("mesh", help="rectangular N x M grid (n >= 2, m >= 2)")
    p_mesh.add_argument("-n", type=int, required=True, help="grid rows (>=2)")
    p_mesh.add_argument("-m", type=int, required=True, help="grid cols (>=2)")
    _add_global_flags(p_mesh)

    p_star = sub.add_parser(
        "star",
        help="star with one center + B branches of P nodes each (b >= 2, p >= 1)",
    )
    p_star.add_argument(
        "-p", type=int, required=True, help="nodes per branch (>=1)"
    )
    p_star.add_argument(
        "-b", type=int, required=True, help="number of branches (>=2)"
    )
    _add_global_flags(p_star)

    # Erdős–Rényi: baseline sin estructura (modelo "no-structure").
    p_er = sub.add_parser(
        "er",
        help="Erdős–Rényi G(N, p) con p = ⟨k⟩/(N-1) — baseline aleatoria",
    )
    p_er.add_argument("-n", type=int, required=True, help="number of nodes (>=2)")
    p_er.add_argument(
        "-k", type=float, required=True, help="target average degree (>0, <= n-1)"
    )
    p_er.add_argument(
        "--seed", type=int, default=None,
        help="random seed for reproducibility (default: nondeterministic)",
    )
    _add_global_flags(p_er)

    # Barabási–Albert: scale-free con vínculo preferencial.
    p_ba = sub.add_parser(
        "ba",
        help="Barabási–Albert scale-free graph (preferential attachment), m = ⟨k⟩/2",
    )
    p_ba.add_argument("-n", type=int, required=True, help="number of nodes (>=2)")
    p_ba.add_argument(
        "-k", type=float, required=True,
        help="target average degree (>=2.0, <= n-1)",
    )
    p_ba.add_argument(
        "--seed", type=int, default=None,
        help="random seed for reproducibility (default: nondeterministic)",
    )
    _add_global_flags(p_ba)

    # Random Geometric Graph: N puntos uniformes en cuadrado, edges si distancia ≤ r.
    p_rgg = sub.add_parser(
        "rgg",
        help=(
            "Random Geometric Graph: N puntos en cuadrado 2D, conectar pares "
            "con distancia <= max_distance_km. Cada arista lleva su distancia real "
            "(capacidad QKD distinta por edge)."
        ),
    )
    p_rgg.add_argument("-n", type=int, required=True, help="number of nodes (>=2)")
    p_rgg.add_argument(
        "--max-distance-km", type=float, required=True,
        dest="max_distance_km",
        help="máxima distancia para conexión (km, >0)",
    )
    p_rgg.add_argument(
        "-k", type=float, required=True,
        help="target average degree (>0); el lado del cuadrado se ajusta para que ⟨k⟩ ≈ target",
    )
    p_rgg.add_argument(
        "--seed", type=int, default=None,
        help="random seed for reproducibility (default: nondeterministic)",
    )
    _add_global_flags(p_rgg)

    # SECOQC partial mesh: anillo + cuerdas hasta ⟨k⟩ objetivo.
    p_secoqc = sub.add_parser(
        "secoqc",
        help=(
            "Malla parcial tipo SECOQC: anillo base de N nodos + cuerdas "
            "aleatorias hasta alcanzar ⟨k⟩ objetivo. Refleja redes QKD "
            "operativas (Vienna, Geneva, Madrid)."
        ),
    )
    p_secoqc.add_argument("-n", type=int, required=True, help="number of nodes (>=3)")
    p_secoqc.add_argument(
        "-k", type=float, required=True,
        help="target average degree (>=2.0, <= n-1)",
    )
    p_secoqc.add_argument(
        "--seed", type=int, default=None,
        help="random seed for reproducibility (default: nondeterministic)",
    )
    _add_global_flags(p_secoqc)

    # ``bridge`` — multi-cluster topology with single-edge bridges between
    # clusters. The "obvious bottleneck" case for multi-path testing.
    p_bridge = sub.add_parser(
        "bridge",
        help=(
            "clusters (rings) joined by single bridge edges — obvious cuello "
            "topológico para tests de robustez de multi-path"
        ),
    )
    p_bridge.add_argument(
        "--cluster-n",
        type=int,
        required=True,
        help="nodes per cluster (>=3, each cluster is a ring)",
    )
    p_bridge.add_argument(
        "--cluster-count",
        type=int,
        default=2,
        help="number of clusters (default 2, must be >=2)",
    )
    _add_global_flags(p_bridge)

    # ``replot`` regenerates plots from the CSVs in an existing run dir
    # — useful when iterating on plot styling without re-running EKS.
    p_replot = sub.add_parser(
        "replot",
        help="re-render plots from CSVs of a previous run (no EKS needed)",
    )
    p_replot.add_argument(
        "output_dir",
        help="path to <run> directory containing data/ subdir from a previous run",
    )
    p_replot.add_argument(
        "--buffer-enc-size",
        type=int,
        default=DEFAULT_BUFFER_SIZE,
        help=f"per-peer ENC buffer capacity used to compute fill ratios (default {DEFAULT_BUFFER_SIZE})",
    )
    p_replot.add_argument(
        "--sat-threshold",
        type=float,
        default=DEFAULT_SAT_THRESHOLD,
        help=f"saturation threshold as fraction of buffer (default {DEFAULT_SAT_THRESHOLD})",
    )

    return parser


def _emit_dry_run(payload: dict[str, Any]) -> int:
    json.dump(payload, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


def _apply_node_id_offset(topology: dict[str, Any], offset: int) -> dict[str, Any]:
    """Shift every node_id by ``offset`` to avoid kme PK collisions.

    The orchestator derives ``local_qkc_id = 100000 + node_id``. If two
    sims in the same DB use overlapping node_ids, the second
    ``create_simulation`` fails with ``NotNullViolation`` during the
    KME upsert. ``--node-id-offset N`` produces ``node_id ∈ [1+N,
    N+N]`` and the resulting uids ``node-{1+N}…node-{N+N}``.

    Mutates ``topology`` in place AND returns it.
    """
    if offset == 0:
        return topology
    if offset < 0:
        raise ValueError(f"node-id-offset must be >= 0, got {offset}")
    rename: dict[str, str] = {}
    for nd in topology["nodes"]:
        old_uid = nd["uid"]
        new_id = nd["node_id"] + offset
        new_uid = f"node-{new_id}"
        rename[old_uid] = new_uid
        nd["node_id"] = new_id
        nd["uid"] = new_uid
        nd["label"] = f"DKMS {new_id}"
    for ln in topology["links"]:
        ln["source_uid"] = rename[ln["source_uid"]]
        ln["target_uid"] = rename[ln["target_uid"]]
        a, b = ln["source_uid"], ln["target_uid"]
        left, right = (a, b) if a <= b else (b, a)
        ln["uid"] = f"edge-{left}-{right}"
    return topology


def _theory_rates(topology: dict[str, Any], alpha: float) -> dict[str, float]:
    """Compute the theoretical max-min rate per commodity for the topology."""
    graph = topology_to_graph(topology)
    caps = topology_to_capacities(topology, alpha=alpha)
    commodities = all_pairs_commodities(graph)
    return weighted_maxmin(commodities, caps)


def _ask_yes_no(prompt: str, *, default_no: bool = True) -> bool:
    """Read a y/N answer from stdin. ``default_no=True`` means an empty
    answer aborts. Falls back to ``False`` if stdin is not a TTY."""
    if not sys.stdin.isatty():
        sys.stderr.write(
            f"{prompt} [stdin not a TTY → assuming No]\n"
        )
        return False
    suffix = "[y/N]" if default_no else "[Y/n]"
    try:
        answer = input(f"{prompt} {suffix} ").strip().lower()
    except EOFError:
        return False
    if not answer:
        return not default_no
    return answer in ("y", "yes")


def _resolve_duplicate_sim(
    client: OrchestatorClient, name: str, *, force: bool
) -> int:
    """Delete sims previously created with ``name`` (after confirming).

    Returns the number of sims deleted (0 if none matched or user
    aborted). Raises ``SystemExit(5)`` if the user declines to delete.
    """
    try:
        sims = client.list_simulations()
    except OrchestatorError as exc:
        sys.stderr.write(
            f"[dkms-topo] WARN: could not list simulations: {exc}\n"
        )
        return 0
    matches = [s for s in sims if s.get("name") == name]
    if not matches:
        return 0
    ids = [s.get("id") for s in matches if s.get("id") is not None]
    sys.stderr.write(
        f"[dkms-topo] found {len(matches)} pre-existing sim(s) named "
        f"{name!r}: ids={ids}\n"
    )
    if not force:
        proceed = _ask_yes_no(
            f"Delete {len(matches)} sim(s) named {name!r}?"
        )
        if not proceed:
            sys.stderr.write(
                "[dkms-topo] aborted — re-run with --force to delete "
                "automatically\n"
            )
            raise SystemExit(5)
    deleted = 0
    for sim_id in ids:
        try:
            client.delete_simulation(int(sim_id))
            deleted += 1
            sys.stderr.write(f"[dkms-topo] deleted sim {sim_id}\n")
        except OrchestatorError as exc:
            sys.stderr.write(
                f"[dkms-topo] WARN: delete sim {sim_id} failed: {exc}\n"
            )
    return deleted


def _login_or_register(
    client: OrchestatorClient, username: str, password: str, email: str
) -> int:
    """Login; if the user does not exist, register and login again."""
    try:
        _, uid = client.login(username, password)
        return uid
    except OrchestatorError:
        try:
            _, uid = client.register_user(username, email, password)
            return uid
        except AlreadyExistsError:
            _, uid = client.login(username, password)
            return uid


def _dynamic_src_for_log(
    log_path: Path, all_uids: set[str]
) -> tuple[str | None, list[dict[str, Any]]]:
    """Resolve the source uid of ``log_path`` by exclusion.

    A DKMS never emits ``generator.state`` events for itself, so the
    only uid that never appears as ``peer`` in the log is the source.
    Returns ``(src_uid_or_None, parsed_events)``; parsed events are
    returned so the caller can reuse them without re-reading the file.
    """
    events: list[dict[str, Any]] = []
    peers_seen: set[str] = set()
    try:
        fh = open(log_path, "r", encoding="utf-8", errors="replace")
    except OSError:
        return None, events
    with fh:
        for raw in fh:
            ev = parse_generator_state(raw)
            if not ev:
                continue
            events.append(ev)
            peer = ev.get("peer")
            if isinstance(peer, str):
                peers_seen.add(peer)
    candidates = all_uids - peers_seen
    if len(candidates) == 1:
        return next(iter(candidates)), events
    return None, events


def _build_log_to_src_map(
    log_dir: Path, all_uids: set[str]
) -> dict[str, str]:
    """Build ``{filename: src_uid}`` for every log that can be disambiguated."""
    out: dict[str, str] = {}
    for log_path in log_dir.glob("*.log"):
        src, _ = _dynamic_src_for_log(log_path, all_uids)
        if src is not None:
            out[log_path.name] = src
    return out


def _all_peers_saturated(
    capture_dir: Path,
    expected_commodities: set[str],
    all_uids: set[str],
    buffer_size: int,
    sat_threshold: float,
) -> tuple[bool, set[str]]:
    """Read every .log in ``capture_dir`` and return ``(all_done, sat_set)``.

    Resolves the src of each log by exclusion against ``all_uids``.
    """
    sat_target = int(round(sat_threshold * buffer_size))
    sat: set[str] = set()
    for log_path in capture_dir.glob("*.log"):
        src, events = _dynamic_src_for_log(log_path, all_uids)
        if not src:
            continue
        for ev in events:
            enc = ev.get("enc")
            peer = ev.get("peer")
            if isinstance(enc, int) and enc >= sat_target and isinstance(peer, str):
                sat.add(f"{src}->{peer}")
    all_done = expected_commodities.issubset(sat)
    return all_done, sat


def _make_output_dir(args: argparse.Namespace, name: str) -> Path:
    if args.output_dir:
        return Path(args.output_dir)
    ts = _dt.datetime.now(_dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    return DEFAULT_RESULTS_BASE / f"{ts}-{name}"


def _resolve_src_for_logs(
    log_dir: Path, fallback_pods: list[str]
) -> dict[str, str | None]:
    """Map ``<pod>.log`` filename → topological src uid via analyze's resolver."""
    from tests.cli.analyze import _default_src_resolver

    out: dict[str, str | None] = {}
    seen_paths = {p.name for p in log_dir.glob("*.log")}
    for pod in fallback_pods:
        seen_paths.add(f"{pod}.log")
    for name in seen_paths:
        out[name] = _default_src_resolver(name)
    return out


def _discover_dkms_ids(namespace: str, kubectl_path: str = "kubectl") -> list[str]:
    """List the ``dkms-<bd_id>`` instance labels of every DKMS pod.

    The Rust DKMS reports ``peer=dkms-<bd_id>`` (database autoincrement)
    in its logs, NOT the editor's ``node-<uid>``. To make the theoretical
    max-min map onto observed events, we must rename theory keys from
    ``node-X`` to ``dkms-Y`` using the natural lexicographic order
    (lowest editor node_id → lowest dkms.id_BD).
    """
    args = [
        kubectl_path, "get", "pods", "-n", namespace,
        "-l", "app=dkms", "-o", "jsonpath={.items[*].metadata.labels.instance}",
    ]
    result = subprocess.run(args, capture_output=True, text=True, timeout=30)
    if result.returncode != 0:
        raise RuntimeError(
            f"kubectl get pods failed (rc={result.returncode}): "
            f"{result.stderr.strip()}"
        )
    return [s for s in result.stdout.strip().split() if s.startswith("dkms-")]


def _build_uid_to_dkms_id_map(
    topology: dict[str, Any], dkms_ids: list[str]
) -> dict[str, str]:
    """Match topology node uids to live dkms_id strings positionally.

    Both are sorted ascending: editor ``node_id`` and BD ``dkms.id``.
    The orchestator's table assigns ids in creation order, which equals
    the ascending node_id traversal in ``_build_model_simulation_from_web``.
    """
    sorted_nodes = sorted(topology["nodes"], key=lambda n: n["node_id"])
    sorted_dkms = sorted(dkms_ids, key=lambda s: int(s.split("-")[1]))
    if len(sorted_dkms) != len(sorted_nodes):
        raise RuntimeError(
            f"dkms pod count {len(sorted_dkms)} != topology nodes "
            f"{len(sorted_nodes)}"
        )
    return {sorted_nodes[i]["uid"]: sorted_dkms[i] for i in range(len(sorted_nodes))}


def _remap_theory_rates(
    theory: dict[str, float], uid_to_dkms: dict[str, str]
) -> dict[str, float]:
    """Re-key ``theory_rates`` from ``node-X->node-Y`` to ``dkms-A->dkms-B``."""
    out: dict[str, float] = {}
    for k, v in theory.items():
        a, b = k.split("->")
        if a in uid_to_dkms and b in uid_to_dkms:
            out[f"{uid_to_dkms[a]}->{uid_to_dkms[b]}"] = v
    return out


def _wait_pods_ready(
    namespace: str,
    selector: str,
    timeout_seconds: float,
    *,
    kubectl_path: str = "kubectl",
) -> None:
    """Block until every pod matching ``selector`` in ``namespace`` is Ready.

    Wraps ``kubectl wait --for=condition=Ready --timeout=Ns pod -l ...``.
    Raises ``RuntimeError`` on rc != 0 (typically timeout).
    """
    args = [
        kubectl_path,
        "wait",
        "-n",
        namespace,
        "--for=condition=Ready",
        f"--timeout={int(timeout_seconds)}s",
        "pod",
        "-l",
        selector,
    ]
    result = subprocess.run(
        args, capture_output=True, text=True, timeout=timeout_seconds + 30
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"kubectl wait pods not Ready (rc={result.returncode}): "
            f"{result.stderr.strip() or result.stdout.strip()}"
        )


def _stage_saturate(
    args: argparse.Namespace,
    namespace: str,
    output_dir: Path,
    theory_node: dict[str, float],
    topology: dict[str, Any],
) -> int:
    """Tail DKMS pod logs and wait until every commodity saturates.

    Returns 0 on full saturation, 3 on timeout. Other exceptions bubble.
    """
    sys.stderr.write(
        f"[dkms-topo] waiting for pods Ready in {namespace} "
        f"(selector={args.pod_selector}, timeout={int(args.pod_ready_timeout)}s)\n"
    )
    try:
        _wait_pods_ready(
            namespace,
            args.pod_selector,
            args.pod_ready_timeout,
        )
    except RuntimeError as exc:
        sys.stderr.write(f"[dkms-topo] WARN pods not Ready in time: {exc}\n")
        # Continue anyway — maybe partial pods are Ready and we can capture
        # something useful.

    # Re-key theory_rates from editor `node-<uid>` to live `dkms-<bd_id>` so
    # commodity ids match what the Rust DKMS emits in `generator.state peer=...`.
    try:
        dkms_ids = _discover_dkms_ids(namespace)
        uid_to_dkms = _build_uid_to_dkms_id_map(topology, dkms_ids)
        theory = _remap_theory_rates(theory_node, uid_to_dkms)
        sys.stderr.write(
            f"[dkms-topo] mapped {len(uid_to_dkms)} editor uids to dkms ids: "
            f"{sorted(uid_to_dkms.items())[:4]}{'...' if len(uid_to_dkms) > 4 else ''}\n"
        )
    except RuntimeError as exc:
        sys.stderr.write(
            f"[dkms-topo] WARN could not remap theory to dkms ids: {exc}\n"
        )
        theory = theory_node
    expected = set(theory.keys())
    all_uids: set[str] = set()
    for k in expected:
        a, b = k.split("->")
        all_uids.add(a)
        all_uids.add(b)
    sys.stderr.write(
        f"[dkms-topo] expected commodities={len(expected)} uids={sorted(all_uids)}\n"
    )
    stop_event = threading.Event()
    sys.stderr.write(
        f"[dkms-topo] tailing dkms pods in {namespace} → {output_dir}\n"
    )
    capture = tail_pods(
        namespace,
        "dkms-",
        output_dir,
        stop_event=stop_event,
        container="dkms",
    )
    rc = 0
    try:
        deadline = time.monotonic() + args.saturation_timeout
        sys.stderr.write("[dkms-topo] waiting for all peers to saturate\n")
        while time.monotonic() < deadline:
            time.sleep(DEFAULT_POLL_INTERVAL_S)
            done, sat_set = _all_peers_saturated(
                output_dir,
                expected,
                all_uids,
                args.buffer_enc_size,
                args.sat_threshold,
            )
            sys.stderr.write(
                f"[dkms-topo]   {len(sat_set)}/{len(expected)} commodities saturated\n"
            )
            if done:
                sys.stderr.write("[dkms-topo] ALL saturated\n")
                break
        else:
            sys.stderr.write(
                "[dkms-topo] timeout reached before full saturation\n"
            )
            rc = 3
    finally:
        stop_event.set()
        try:
            capture.join(timeout=15)
        except Exception:
            pass

    log_to_src = _build_log_to_src_map(output_dir, all_uids)
    sys.stderr.write(
        f"[dkms-topo] resolved src for {len(log_to_src)}/{len(list(output_dir.glob('*.log')))} log files\n"
    )

    def _resolver(fn: str) -> str | None:
        return log_to_src.get(fn)

    sys.stderr.write("[dkms-topo] running saturation analysis\n")
    result = analyze_saturation(
        output_dir,
        buffer_size=args.buffer_enc_size,
        theory_rates=theory,
        sat_threshold=args.sat_threshold,
        src_resolver=_resolver,
        write_csv=getattr(args, "write_csv", True),
        write_plots=getattr(args, "write_plots", True),
    )
    s = result["summary"]
    sys.stderr.write(
        f"[dkms-topo] sat={s['saturated_count']}/{s['total_count']} "
        f"median_ratio={s.get('median_ratio')!r}\n"
    )
    if result.get("csvs"):
        sys.stderr.write(
            f"[dkms-topo] wrote {len(result['csvs'])} CSVs to {output_dir}/data/\n"
        )
    if result.get("plots"):
        sys.stderr.write(
            f"[dkms-topo] wrote {len(result['plots'])} plots to {output_dir}/plots/\n"
        )
    return rc


def _compute_loadtest_duration(args: argparse.Namespace) -> float:
    """Estimate a sane loadtest wall-clock if the user didn't pass one.

    Ramp time = warmup + ((end - start) / step) * interval. Add 60s margin.
    """
    if args.loadtest_duration is not None:
        return args.loadtest_duration
    steps = max(0, (args.sae_end - args.sae_start)) // max(1, args.sae_step)
    return args.sae_warmup + steps * args.time_step + 60.0


def _stage_sae_test(
    args: argparse.Namespace,
    client: OrchestatorClient,
    sim_id: int,
    namespace: str,
    output_dir: Path,
) -> int:
    """Submit a SAE ramp loadtest, capture logs+metrics, analyze."""
    params: dict[str, Any] = {
        "start_saes": args.sae_start,
        "end_saes": args.sae_end,
        "step_saes": args.sae_step,
        "interval_seconds": args.time_step,
        "warmup_seconds": args.sae_warmup,
        "per_sae_lambda": args.lambda_sae,
        "key_size_bits": args.sae_key_size,
        "request_timeout_seconds": args.sae_request_timeout,
    }
    duration = _compute_loadtest_duration(args)
    sys.stderr.write(
        f"[dkms-topo] submitting loadtest with ramp "
        f"{args.sae_start}->{args.sae_end} step {args.sae_step}, "
        f"interval {args.time_step}s, λ={args.lambda_sae} "
        f"(total {duration:.0f}s)\n"
    )
    out = submit_and_wait_loadtest(
        client,
        sim_id,
        params,
        output_dir,
        total_timeout_seconds=duration,
        namespace=namespace,
    )
    metrics_path = out["metrics_path"]
    if not out.get("scrape_ok"):
        sys.stderr.write(
            f"[dkms-topo] loadtest scrape FAILED: {out.get('scrape_error')}\n"
        )
        return 4
    text = Path(metrics_path).read_text(encoding="utf-8")
    metrics = parse_metrics(text)
    requests_csv = out.get("requests_csv")
    summary = analyze_loadtest(
        metrics,
        params,
        output_dir=output_dir,
        write_csv=getattr(args, "write_csv", True),
        write_plots=getattr(args, "write_plots", True),
        requests_csv=requests_csv,
    )
    total = summary["totals"]["requests"]
    pct = summary["percentages"]
    sys.stderr.write(
        f"[dkms-topo] loadtest done: total={int(total)} "
        f"ok={pct.get('ok', 0):.1f}% throttled={pct.get('throttled', 0):.1f}% "
        f"errors={pct.get('client_error', 0) + pct.get('server_error', 0):.1f}%\n"
    )
    if requests_csv is not None:
        sys.stderr.write(
            f"[dkms-topo] extracted requests.csv from loadtest pod → {requests_csv}\n"
        )
    if summary.get("csvs"):
        sys.stderr.write(
            f"[dkms-topo] wrote {len(summary['csvs'])} loadtest CSVs\n"
        )
    if summary.get("plots"):
        sys.stderr.write(
            f"[dkms-topo] wrote {len(summary['plots'])} loadtest plots\n"
        )
    return 0


def _run_eks_session(
    args: argparse.Namespace,
    payload: dict[str, Any],
    topology: dict[str, Any],
) -> int:
    """Lifecycle: login → create_sim → run_sim → [stages…] → stop_sim.

    Stop is ALWAYS attempted (R-008). Stages run sequentially; the
    first non-zero return becomes the overall ``rc``.
    """
    output_dir = _make_output_dir(args, payload["name"])
    output_dir.mkdir(parents=True, exist_ok=True)
    (output_dir / "payload.json").write_text(
        json.dumps(payload, indent=2), encoding="utf-8"
    )

    client = OrchestatorClient(
        authz_url=args.authz_url, orch_url=args.orch_url
    )
    sys.stderr.write(f"[dkms-topo] authz login as {args.username}\n")
    _login_or_register(
        client, args.username, args.password, f"{args.username}@dkms-topo.local"
    )

    force_or_yes = bool(args.force or args.assume_yes)
    _resolve_duplicate_sim(client, payload["name"], force=force_or_yes)

    sys.stderr.write("[dkms-topo] creating simulation\n")
    sim_id = client.create_simulation(payload)
    # Orchestator names the namespace literally `<sim_id>` (see
    # orchestrator/pods.py:553 -> `self.namespace = str(id_simulation)`).
    namespace = args.namespace or str(sim_id)
    sys.stderr.write(f"[dkms-topo] sim_id={sim_id} namespace={namespace}\n")

    theory_node = _theory_rates(topology, args.effective_alpha)
    do_saturate = args.buffer_saturated or (args.sae_test and not args.no_fill)
    do_sae_test = args.sae_test

    rc = 0
    try:
        sys.stderr.write("[dkms-topo] starting simulation\n")
        client.run_simulation(sim_id)

        if do_saturate:
            sat_rc = _stage_saturate(args, namespace, output_dir, theory_node, topology)
            if sat_rc != 0 and rc == 0:
                rc = sat_rc

        if do_sae_test:
            sae_rc = _stage_sae_test(args, client, sim_id, namespace, output_dir)
            if sae_rc != 0 and rc == 0:
                rc = sae_rc
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"[dkms-topo] ERROR: {exc}\n")
        rc = 1
    finally:
        try:
            sys.stderr.write(f"[dkms-topo] stopping sim {sim_id}\n")
            client.stop_simulation(sim_id)
        except Exception as exc:  # noqa: BLE001
            sys.stderr.write(f"[dkms-topo] stop_simulation failed: {exc}\n")
    return rc


def _run_replot(args: argparse.Namespace) -> int:
    """Re-render saturation plots from the CSVs of a previous run.

    Reads ``<output_dir>/data/{generator_state,per_commodity,theory_rates}.csv``,
    reconstructs the in-memory structures, and calls the plot helpers.
    """
    import csv as _csv
    from datetime import datetime as _datetime

    from tests.cli.analyze import (
        _plot_emit_rate,
        _plot_enc_over_time,
        _plot_rate_vs_theory,
        _plot_saturation_ratios,
    )

    out_dir = Path(args.output_dir)
    data_dir = out_dir / "data"
    if not data_dir.exists():
        sys.stderr.write(f"error: no data/ subdir found in {out_dir}\n")
        return 1

    gen_csv = data_dir / "generator_state.csv"
    pc_csv = data_dir / "per_commodity.csv"
    th_csv = data_dir / "theory_rates.csv"
    if not gen_csv.exists():
        sys.stderr.write(f"error: {gen_csv} missing\n")
        return 1

    # generator_state.csv -> events_by_src_peer + global t0
    events_by_src_peer: dict[str, dict[str, list[dict[str, Any]]]] = {}
    t0: _datetime | None = None
    with open(gen_csv, "r", encoding="utf-8") as fh:
        for row in _csv.DictReader(fh):
            t_iso = row.get("t_log_iso") or ""
            try:
                t_log = _datetime.fromisoformat(t_iso) if t_iso else None
            except ValueError:
                t_log = None
            src = row.get("src") or ""
            peer = row.get("peer") or ""
            if not src or not peer:
                continue

            def _coerce_int(v: str | None) -> int | None:
                if v is None or v == "":
                    return None
                try:
                    return int(v)
                except ValueError:
                    return None

            def _coerce_float(v: str | None) -> float | None:
                if v is None or v == "":
                    return None
                try:
                    return float(v)
                except ValueError:
                    return None

            ev = {
                "t_log": t_log,
                "enc": _coerce_int(row.get("enc")),
                "dec": _coerce_int(row.get("dec")),
                "ack_pending": _coerce_int(row.get("ack_pending")),
                "emit_total": _coerce_int(row.get("emit_total")),
                "observed_keys_per_s": _coerce_float(row.get("observed_keys_per_s")),
                "sdn_rate_keys_per_s": _coerce_float(row.get("sdn_rate_keys_per_s")),
            }
            events_by_src_peer.setdefault(src, {}).setdefault(peer, []).append(ev)
            if t_log is not None and (t0 is None or t_log < t0):
                t0 = t_log

    # per_commodity.csv -> list of dicts (numeric fields coerced)
    per_commodity: list[dict[str, Any]] = []
    summary: dict[str, Any] = {}
    if pc_csv.exists():
        with open(pc_csv, "r", encoding="utf-8") as fh:
            for row in _csv.DictReader(fh):
                parsed: dict[str, Any] = {}
                for k, v in row.items():
                    if v == "" or v is None:
                        parsed[k] = None
                    elif k in ("saturated",):
                        parsed[k] = v.lower() in ("true", "1", "yes")
                    else:
                        try:
                            parsed[k] = int(v)
                        except ValueError:
                            try:
                                parsed[k] = float(v)
                            except ValueError:
                                parsed[k] = v
                per_commodity.append(parsed)
        ratios = [r["ratio"] for r in per_commodity if isinstance(r.get("ratio"), float)]
        sats = [r for r in per_commodity if r.get("saturated")]
        summary = {
            "saturated_count": len(sats),
            "total_count": len(per_commodity),
            "ratio_count": len(ratios),
        }
        if ratios:
            ratios_sorted = sorted(ratios)
            mid = len(ratios_sorted) // 2
            median = (
                ratios_sorted[mid]
                if len(ratios_sorted) % 2 == 1
                else 0.5 * (ratios_sorted[mid - 1] + ratios_sorted[mid])
            )
            summary["median_ratio"] = median

    # theory_rates.csv -> dict[commodity_id, rate]
    theory_rates: dict[str, float] = {}
    if th_csv.exists():
        with open(th_csv, "r", encoding="utf-8") as fh:
            for row in _csv.DictReader(fh):
                cid = row.get("commodity_id") or ""
                try:
                    rate = float(row.get("theory_rate_kps") or 0.0)
                except ValueError:
                    continue
                if cid:
                    theory_rates[cid] = rate

    plot_dir = out_dir / "plots"
    written: list[str] = []
    enc_plot = plot_dir / "sat_enc_over_time.png"
    if _plot_enc_over_time(
        events_by_src_peer, args.buffer_enc_size, args.sat_threshold, t0, enc_plot
    ):
        written.append(str(enc_plot))
    ratio_plot = plot_dir / "sat_ratios.png"
    if _plot_saturation_ratios(per_commodity, summary, ratio_plot):
        written.append(str(ratio_plot))
    rate_plot = plot_dir / "sat_emit_rate.png"
    if _plot_emit_rate(events_by_src_peer, t0, rate_plot):
        written.append(str(rate_plot))
    rate_vs_theory_plot = plot_dir / "sat_rate_vs_theory.png"
    if _plot_rate_vs_theory(per_commodity, theory_rates, rate_vs_theory_plot):
        written.append(str(rate_vs_theory_plot))

    sys.stderr.write(f"[dkms-topo] replot wrote {len(written)} plots under {plot_dir}\n")
    for p in written:
        sys.stderr.write(f"  - {p}\n")
    if not written:
        sys.stderr.write("warning: no plots produced (matplotlib missing or empty data)\n")
        return 1
    return 0


def run(argv: list[str] | None = None) -> int:
    """Entry point usable as a function (returns an exit code).

    EKS path (no --dry-run) is **not implemented in this iteration** — the
    flow needs ``--buffer-saturated`` / ``--sae-test`` orchestration that
    lands in OBJ-018 / OBJ-019. Until then, callers must pass ``--dry-run``
    or the CLI exits with code 2 and a "not implemented" message.
    """
    parser = _build_parser()
    args = parser.parse_args(argv)

    if args.topology == "replot":
        return _run_replot(args)

    try:
        topo = _build_topology(args.topology, args)
        _apply_node_id_offset(topo, args.node_id_offset)
    except (ValueError, RuntimeError) as exc:
        sys.stderr.write(f"error: {exc}\n")
        return 1

    name = args.name or _auto_name(args.topology, args)
    payload = build_sim_payload(
        topo,
        name=name,
        owner_uid=args.owner,
        sdn_endpoint=args.sdn_endpoint,
        r0=args.r0,
        alpha=args.alpha,
        distance_km=args.distance,
        buffer_max=args.buffer_enc_size,
    )

    if args.dry_run:
        return _emit_dry_run(payload)

    if args.buffer_saturated or args.sae_test:
        return _run_eks_session(args, payload, topo)

    sys.stderr.write(
        "error: pass --dry-run (offline), --buffer-saturated and/or "
        "--sae-test (run on EKS).\n"
    )
    return 2


def main() -> None:  # pragma: no cover
    sys.exit(run())


if __name__ == "__main__":  # pragma: no cover
    main()
