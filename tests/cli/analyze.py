"""Saturation-time analysis from per-pod DKMS logs.

Walks a directory of ``<pod>.log`` files (output of
``tests/cli/log_capture.tail_pods``), parses each ``generator.state``
event with ``sat_parser.parse_generator_state``, and computes the
time-to-saturation per ``(src, peer)`` commodity. Compares against a
theoretical baseline (``buffer_size / theory_rates[commodity]``) and
reports the median / quartiles of the observed/theoretical ratio.

The output JSON is written to ``<log_dir>/sat_analysis.json`` and the
function also returns it as a dict, so callers can render plots or
exit-code on a quality threshold.

Saturation criterion: an ENC buffer is considered saturated at the
first event with ``enc >= sat_threshold * buffer_size`` (default
``0.95``). The default matches CLAUDE.md section "DKMS Rust /metrics
endpoint" and the previous plot scripts.
"""

from __future__ import annotations

import json
import re
import statistics
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from tests.cli.sat_parser import parse_generator_state

DEFAULT_SAT_THRESHOLD: float = 0.95


def _default_src_resolver(filename: str) -> str | None:
    """Infer the topological src uid from a log filename.

    Strategy:

    * Strip the ``.log`` suffix.
    * Look for the rightmost ``-<digits>`` group; this works for both
      bare topology uids (``node-3.log``) and Kubernetes pod names
      (``dkms-30-9d87758d8-phswx.log`` → captures ``30``).
    * Return ``f"node-{N}"`` to match the builder convention.

    Returns ``None`` if no digit group is found.
    """
    stem = filename
    if stem.endswith(".log"):
        stem = stem[:-4]
    # Pod names usually look like `dkms-<id>-<replicaset-hash>-<pod-suffix>`;
    # the id is the first numeric run we find.
    m = re.search(r"^(?:dkms-)?(?:[a-zA-Z]+-)?(\d+)", stem)
    if m:
        return f"node-{m.group(1)}"
    # Last-chance: any digit group.
    m = re.search(r"(\d+)", stem)
    if m:
        return f"node-{m.group(1)}"
    return None


def _parse_t_log(value: str | None) -> datetime | None:
    if not value:
        return None
    raw = value.rstrip("Z")
    try:
        return datetime.fromisoformat(raw)
    except ValueError:
        return None


def _summarize_ratios(values: list[float]) -> dict[str, float | None]:
    """Compute median/p25/p75 of a list of positive floats."""
    if not values:
        return {"median": None, "p25": None, "p75": None}
    vs = sorted(values)
    n = len(vs)
    median = statistics.median(vs)
    # statistics.quantiles with n=4 gives quartile cuts; index 0 is p25, 2 is p75
    if n >= 2:
        q = statistics.quantiles(vs, n=4, method="inclusive")
        p25 = q[0]
        p75 = q[2]
    else:
        p25 = p75 = vs[0]
    return {"median": median, "p25": p25, "p75": p75}


def _analyze_events_for_src(
    src: str,
    events: list[dict[str, Any]],
    buffer_size: int,
    theory_rates: dict[str, float],
    sat_threshold: float,
) -> list[dict[str, Any]]:
    """Walk parsed events from one log and emit per-peer rows."""
    sat_target = int(round(sat_threshold * buffer_size))

    by_peer: dict[str, dict[str, Any]] = {}
    for ev in events:
        peer = ev.get("peer")
        if not isinstance(peer, str):
            continue
        t_log = _parse_t_log(ev.get("t_log"))
        state = by_peer.setdefault(
            peer,
            {
                "first_emit": None,
                "first_sat": None,
                "first_emit_total": None,
                "enc_at_sat": None,
                "samples": 0,
                "max_enc": 0,
                "last_sdn_rate": None,
                "last_observed_rate": None,
            },
        )
        state["samples"] += 1
        enc = ev.get("enc")
        emit_total = ev.get("emit_total")
        if (
            state["first_emit"] is None
            and emit_total is not None
            and emit_total > 0
        ):
            state["first_emit"] = t_log
            state["first_emit_total"] = emit_total
        if state["first_sat"] is None and enc is not None and enc >= sat_target:
            state["first_sat"] = t_log
            state["enc_at_sat"] = enc
        if isinstance(enc, int) and enc > state["max_enc"]:
            state["max_enc"] = enc
        if "sdn_rate_keys_per_s" in ev:
            state["last_sdn_rate"] = ev["sdn_rate_keys_per_s"]
        if "observed_keys_per_s" in ev:
            state["last_observed_rate"] = ev["observed_keys_per_s"]

    out: list[dict[str, Any]] = []
    for peer, state in by_peer.items():
        commodity_id = f"{src}->{peer}"
        theory_rate = theory_rates.get(commodity_id)
        t_first = state["first_emit"]
        t_sat = state["first_sat"]
        saturated = t_sat is not None
        if saturated and t_first is not None:
            t_observed = (t_sat - t_first).total_seconds()
        elif saturated:
            t_observed = 0.0
        else:
            t_observed = None
        if theory_rate is not None and theory_rate > 0:
            t_theor = buffer_size / theory_rate
        else:
            t_theor = None
        if (
            t_observed is not None
            and t_theor is not None
            and t_theor > 0
        ):
            ratio = t_observed / t_theor
        else:
            ratio = None

        out.append(
            {
                "src": src,
                "peer": peer,
                "commodity_id": commodity_id,
                "samples": state["samples"],
                "max_enc": state["max_enc"],
                "enc_at_sat": state["enc_at_sat"],
                "t_first_emit": t_first.isoformat() if t_first else None,
                "t_saturated": t_sat.isoformat() if t_sat else None,
                "t_observed_seconds": t_observed,
                "t_theoretical_seconds": t_theor,
                "ratio": ratio,
                "saturated": saturated,
                "theory_rate_keys_per_s": theory_rate,
                "last_sdn_rate_keys_per_s": state["last_sdn_rate"],
                "last_observed_keys_per_s": state["last_observed_rate"],
            }
        )
    return out


def analyze_saturation(
    log_dir: str | Path,
    buffer_size: int,
    theory_rates: dict[str, float],
    *,
    sat_threshold: float = DEFAULT_SAT_THRESHOLD,
    src_resolver: Callable[[str], str | None] | None = None,
    output_file: str | Path | None = None,
) -> dict[str, Any]:
    """Compute saturation timings across every ``.log`` in ``log_dir``.

    Args:
        log_dir: Directory of per-pod ``.log`` files.
        buffer_size: Configured ENC buffer capacity per peer.
        theory_rates: Map ``"src->peer"`` → rate (keys/s) from the
            theoretical max-min model
            (``maxmin_theory.weighted_maxmin(...)``).
        sat_threshold: Fraction of ``buffer_size`` for saturation
            (default ``0.95``).
        src_resolver: ``filename -> src_uid``. Default infers
            ``node-<N>`` from the first digit group of the filename.
        output_file: Path for the JSON output. Default
            ``<log_dir>/sat_analysis.json``.

    Returns:
        ``{"per_commodity": [...], "summary": {...}, "buffer_size":
        ..., "sat_threshold": ...}``.
    """
    log_dir = Path(log_dir)
    if not log_dir.exists():
        raise FileNotFoundError(f"log_dir does not exist: {log_dir}")
    if buffer_size <= 0:
        raise ValueError(f"buffer_size must be > 0, got {buffer_size}")
    if not 0 < sat_threshold <= 1:
        raise ValueError(
            f"sat_threshold must be in (0, 1], got {sat_threshold}"
        )
    resolver = src_resolver or _default_src_resolver

    per_commodity: list[dict[str, Any]] = []
    log_files = sorted(log_dir.glob("*.log"))
    for log_file in log_files:
        src = resolver(log_file.name)
        if not src:
            continue
        events: list[dict[str, Any]] = []
        with open(log_file, "r", encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                parsed = parse_generator_state(raw)
                if parsed is not None:
                    events.append(parsed)
        per_commodity.extend(
            _analyze_events_for_src(
                src, events, buffer_size, theory_rates, sat_threshold
            )
        )

    saturated_rows = [r for r in per_commodity if r["saturated"]]
    ratios = [
        r["ratio"] for r in per_commodity if isinstance(r["ratio"], float)
    ]
    summary = {
        "saturated_count": len(saturated_rows),
        "total_count": len(per_commodity),
        "ratio_count": len(ratios),
        **{f"{k}_ratio": v for k, v in _summarize_ratios(ratios).items()},
    }

    result = {
        "buffer_size": buffer_size,
        "sat_threshold": sat_threshold,
        "log_files_count": len(log_files),
        "per_commodity": per_commodity,
        "summary": summary,
    }

    out_path = Path(output_file) if output_file else log_dir / "sat_analysis.json"
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as fh:
        json.dump(result, fh, indent=2, default=str)

    return result


__all__ = [
    "DEFAULT_SAT_THRESHOLD",
    "analyze_saturation",
]
