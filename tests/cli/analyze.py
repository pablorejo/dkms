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

import csv
import json
import re
import statistics
from datetime import datetime
from pathlib import Path
from typing import Any, Callable

from tests.cli.sat_parser import parse_generator_state

DEFAULT_SAT_THRESHOLD: float = 0.95

# Hex colors reused across plots (consistent palette with loadtest_analyze).
_COLOR_OK = "#2ecc71"       # green: ratio inside 0.9..1.2
_COLOR_WARN = "#e67e22"     # orange: ratio outside the band
_COLOR_NEUTRAL = "#7f8c8d"  # grey: not saturated (ratio None)
_COLOR_LINE = "#2980b9"     # blue: primary data
_COLOR_REF = "#34495e"      # dark grey: theoretical reference
_COLOR_ANNOT = "#c0392b"    # red: median / annotations
_COLOR_SDN = "#9b59b6"      # purple: SDN-dictated rate overlay


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
) -> tuple[list[dict[str, Any]], dict[str, list[dict[str, Any]]]]:
    """Walk parsed events from one log and emit per-peer rows.

    Returns ``(per_peer_rows, events_by_peer)``:

    * ``per_peer_rows`` is the aggregated summary (one row per peer,
      same shape as before this refactor) consumed by
      ``analyze_saturation`` to build ``per_commodity``.
    * ``events_by_peer`` is the chronologically-sorted raw event
      stream (peer → list of dicts with ``t_log``, ``enc``, ``dec``,
      ``ack_pending``, ``emit_total``, ``observed_keys_per_s``,
      ``sdn_rate_keys_per_s``) used for the CSV export and time-series
      plots. The events are returned in the order they appear in the
      log; callers that need them sorted should re-sort.
    """
    sat_target = int(round(sat_threshold * buffer_size))

    by_peer: dict[str, dict[str, Any]] = {}
    events_by_peer: dict[str, list[dict[str, Any]]] = {}
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

        # Snapshot the raw event for the CSV export and time-series
        # plots. We only keep the fields the downstream plots need.
        events_by_peer.setdefault(peer, []).append(
            {
                "t_log": t_log,
                "enc": ev.get("enc"),
                "dec": ev.get("dec"),
                "ack_pending": ev.get("ack_pending"),
                "emit_total": ev.get("emit_total"),
                "observed_keys_per_s": ev.get("observed_keys_per_s"),
                "sdn_rate_keys_per_s": ev.get("sdn_rate_keys_per_s"),
            }
        )

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
    return out, events_by_peer


# ---------------------------------------------------------------------------
# CSV exporters — stdlib only (csv module). Always safe to call.
# ---------------------------------------------------------------------------


def _write_generator_state_csv(
    events_by_src_peer: dict[str, dict[str, list[dict[str, Any]]]],
    buffer_size: int,
    t0: datetime | None,
    output_path: Path,
) -> bool:
    """Flatten every parsed ``generator.state`` event into one CSV.

    Columns: t_log_iso, t_seconds (from ``t0``), src, peer,
    commodity_id, enc, dec, ack_pending, emit_total, observed_kps,
    sdn_kps, fill_ratio.

    Returns ``True`` if a file was written (1+ row), ``False`` if the
    dataset was empty.
    """
    rows = 0
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with open(output_path, "w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh)
        w.writerow(
            [
                "t_log_iso",
                "t_seconds",
                "src",
                "peer",
                "commodity_id",
                "enc",
                "dec",
                "ack_pending",
                "emit_total",
                "observed_keys_per_s",
                "sdn_rate_keys_per_s",
                "fill_ratio",
            ]
        )
        for src in sorted(events_by_src_peer.keys()):
            for peer in sorted(events_by_src_peer[src].keys()):
                for ev in events_by_src_peer[src][peer]:
                    t = ev.get("t_log")
                    iso = t.isoformat() if t else ""
                    t_sec = (t - t0).total_seconds() if (t and t0) else ""
                    enc = ev.get("enc")
                    fill = (
                        f"{enc / buffer_size:.6f}"
                        if isinstance(enc, int) and buffer_size > 0
                        else ""
                    )
                    w.writerow(
                        [
                            iso,
                            t_sec if t_sec == "" else f"{t_sec:.6f}",
                            src,
                            peer,
                            f"{src}->{peer}",
                            enc if enc is not None else "",
                            ev.get("dec") if ev.get("dec") is not None else "",
                            ev.get("ack_pending") if ev.get("ack_pending") is not None else "",
                            ev.get("emit_total") if ev.get("emit_total") is not None else "",
                            ev.get("observed_keys_per_s") if ev.get("observed_keys_per_s") is not None else "",
                            ev.get("sdn_rate_keys_per_s") if ev.get("sdn_rate_keys_per_s") is not None else "",
                            fill,
                        ]
                    )
                    rows += 1
    return rows > 0


def _write_per_commodity_csv(
    per_commodity: list[dict[str, Any]], output_path: Path
) -> bool:
    """Flatten ``sat_analysis.per_commodity`` into a CSV (one row per commodity)."""
    if not per_commodity:
        return False
    output_path.parent.mkdir(parents=True, exist_ok=True)
    cols = list(per_commodity[0].keys())
    with open(output_path, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=cols, extrasaction="ignore")
        w.writeheader()
        for row in per_commodity:
            w.writerow({k: ("" if row.get(k) is None else row.get(k)) for k in cols})
    return True


def _write_theory_rates_csv(
    theory_rates: dict[str, float], buffer_size: int, output_path: Path
) -> bool:
    """Persist the theoretical max-min rates with the derived t_saturate.

    ``t_saturate = buffer_size / theory_rate`` — overlay material for the
    "rate vs theory" plot and any local pandas analysis.
    """
    if not theory_rates:
        return False
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with open(output_path, "w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh)
        w.writerow(["commodity_id", "src", "peer", "theory_rate_kps", "theory_t_saturate_seconds"])
        for cid in sorted(theory_rates.keys()):
            rate = theory_rates[cid]
            if "->" in cid:
                src, peer = cid.split("->", 1)
            else:
                src, peer = "", ""
            t_sat = buffer_size / rate if rate > 0 else ""
            w.writerow(
                [
                    cid,
                    src,
                    peer,
                    f"{rate:.6f}",
                    f"{t_sat:.6f}" if isinstance(t_sat, float) else t_sat,
                ]
            )
    return True


# ---------------------------------------------------------------------------
# Plot functions — lazy matplotlib import, silent skip on failure.
# ---------------------------------------------------------------------------


def _import_matplotlib():
    """Try to import matplotlib with the Agg backend. Returns ``plt`` or ``None``."""
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore

        return plt
    except Exception:  # noqa: BLE001
        return None


def _commodity_color_cycle(plt, n: int) -> list[str]:
    """Return ``n`` distinct colors from matplotlib's qualitative cycle."""
    if n <= 10:
        return [f"C{i}" for i in range(n)]
    cmap = plt.get_cmap("tab20")
    return [cmap(i % 20) for i in range(n)]


def _plot_enc_over_time(
    events_by_src_peer: dict[str, dict[str, list[dict[str, Any]]]],
    buffer_size: int,
    sat_threshold: float,
    t0: datetime | None,
    output_path: Path,
) -> bool:
    """One line per commodity tracking ``enc`` over time."""
    plt = _import_matplotlib()
    if plt is None:
        return False
    if not events_by_src_peer or t0 is None:
        return False
    series: list[tuple[str, list[float], list[int]]] = []
    for src in sorted(events_by_src_peer.keys()):
        for peer in sorted(events_by_src_peer[src].keys()):
            xs: list[float] = []
            ys: list[int] = []
            for ev in events_by_src_peer[src][peer]:
                t = ev.get("t_log")
                enc = ev.get("enc")
                if t is None or not isinstance(enc, int):
                    continue
                xs.append((t - t0).total_seconds())
                ys.append(enc)
            if xs:
                series.append((f"{src}→{peer}", xs, ys))
    if not series:
        return False
    n = len(series)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(10, 6))
    colors = _commodity_color_cycle(plt, n)
    for (label, xs, ys), c in zip(series, colors):
        ax.plot(xs, ys, color=c, linewidth=1.2, label=label, alpha=0.85)
    target = sat_threshold * buffer_size
    ax.axhline(
        target,
        color=_COLOR_REF,
        linestyle="--",
        linewidth=1.0,
        label=f"sat threshold ({sat_threshold:.2f}×cap = {int(target)})",
    )
    ax.set_xlabel("seconds since first event")
    ax.set_ylabel("enc (keys in ENC buffer)")
    ax.set_title(f"ENC buffer fill — {n} commodities")
    ax.set_ylim(bottom=0)
    if n <= 16:
        ax.legend(loc="lower right", fontsize=8, ncol=2)
    fig.tight_layout()
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _plot_saturation_ratios(
    per_commodity: list[dict[str, Any]],
    summary: dict[str, Any],
    output_path: Path,
) -> bool:
    """Horizontal bar chart of ratio observed/theoretical per commodity."""
    plt = _import_matplotlib()
    if plt is None:
        return False
    if not per_commodity:
        return False
    items: list[tuple[str, float | None]] = []
    for row in per_commodity:
        cid = row.get("commodity_id") or f"{row.get('src')}->{row.get('peer')}"
        ratio = row.get("ratio")
        items.append((cid, ratio if isinstance(ratio, float) else None))
    # Sort: None last; finite ratios ascending.
    items.sort(key=lambda it: (it[1] is None, it[1] if it[1] is not None else 0.0))
    n = len(items)
    labels = [it[0] for it in items]
    values = [it[1] if it[1] is not None else 0.0 for it in items]
    colors = []
    for _, r in items:
        if r is None:
            colors.append(_COLOR_NEUTRAL)
        elif 0.9 <= r <= 1.2:
            colors.append(_COLOR_OK)
        else:
            colors.append(_COLOR_WARN)
    height = min(40.0, max(4.0, 0.35 * n))
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(8, height))
    ax.barh(range(n), values, color=colors)
    ax.set_yticks(range(n))
    ax.set_yticklabels(labels, fontsize=8)
    ax.invert_yaxis()
    ax.axvline(1.0, color=_COLOR_REF, linewidth=1.0)
    median = summary.get("median_ratio")
    if isinstance(median, float):
        ax.axvline(
            median,
            color=_COLOR_ANNOT,
            linestyle="--",
            linewidth=1.0,
            label=f"median={median:.3f}",
        )
        ax.legend(loc="lower right", fontsize=8)
    for i, ((_, r), v) in enumerate(zip(items, values)):
        text = "n/a" if r is None else f"{r:.2f}"
        ax.text(v, i, f" {text}", va="center", fontsize=7)
    s = summary.get("saturated_count", 0)
    t = summary.get("total_count", 0)
    ax.set_xlabel("observed / theoretical saturation time")
    ax.set_title(f"observed/theoretical ratio — sat={s}/{t}")
    fig.tight_layout()
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _plot_emit_rate(
    events_by_src_peer: dict[str, dict[str, list[dict[str, Any]]]],
    t0: datetime | None,
    output_path: Path,
) -> bool:
    """For each commodity, two lines: observed_kps vs sdn_rate_kps over time.

    With many commodities the chart gets busy; we keep one color per
    commodity, solid for observed and dashed for SDN-dictated, and only
    show the legend when n ≤ 8 (otherwise the chart is the legend).
    """
    plt = _import_matplotlib()
    if plt is None:
        return False
    if not events_by_src_peer or t0 is None:
        return False
    series: list[tuple[str, list[float], list[float], list[float]]] = []
    for src in sorted(events_by_src_peer.keys()):
        for peer in sorted(events_by_src_peer[src].keys()):
            xs: list[float] = []
            obs: list[float] = []
            sdn: list[float] = []
            for ev in events_by_src_peer[src][peer]:
                t = ev.get("t_log")
                if t is None:
                    continue
                o = ev.get("observed_keys_per_s")
                s = ev.get("sdn_rate_keys_per_s")
                if not isinstance(o, (int, float)) or not isinstance(s, (int, float)):
                    continue
                xs.append((t - t0).total_seconds())
                obs.append(float(o))
                sdn.append(float(s))
            if xs:
                series.append((f"{src}→{peer}", xs, obs, sdn))
    if not series:
        return False
    n = len(series)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(10, 6))
    colors = _commodity_color_cycle(plt, n)
    for (label, xs, obs, sdn), c in zip(series, colors):
        ax.plot(xs, obs, color=c, linewidth=1.1, alpha=0.85, label=f"{label} obs")
        ax.plot(xs, sdn, color=c, linewidth=0.9, linestyle="--", alpha=0.7)
    ax.set_xlabel("seconds since first event")
    ax.set_ylabel("rate (keys/s)")
    ax.set_title(f"emit rate — observed (solid) vs SDN-dictated (dashed) — {n} commodities")
    ax.set_ylim(bottom=0)
    if n <= 8:
        ax.legend(loc="upper right", fontsize=7, ncol=2)
    fig.tight_layout()
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _plot_rate_vs_theory(
    per_commodity: list[dict[str, Any]],
    theory_rates: dict[str, float],
    output_path: Path,
) -> bool:
    """Scatter of theoretical rate (x) vs SDN-dictated rate observed at
    the end of the run (y). Diagonal y=x is the ideal."""
    plt = _import_matplotlib()
    if plt is None:
        return False
    pts_x: list[float] = []
    pts_y: list[float] = []
    labels: list[str] = []
    for row in per_commodity:
        cid = row.get("commodity_id") or f"{row.get('src')}->{row.get('peer')}"
        tr = theory_rates.get(cid)
        sdn = row.get("last_sdn_rate_keys_per_s")
        if isinstance(tr, (int, float)) and isinstance(sdn, (int, float)) and tr > 0:
            pts_x.append(float(tr))
            pts_y.append(float(sdn))
            labels.append(cid)
    if not pts_x:
        return False
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig, ax = plt.subplots(figsize=(7, 7))
    ax.scatter(pts_x, pts_y, color=_COLOR_LINE, s=40, alpha=0.8, edgecolor=_COLOR_REF)
    lo = min(min(pts_x), min(pts_y))
    hi = max(max(pts_x), max(pts_y))
    pad = (hi - lo) * 0.08 if hi > lo else 1.0
    lo -= pad
    hi += pad
    ax.plot([lo, hi], [lo, hi], color=_COLOR_REF, linestyle="--", linewidth=1.0, label="y = x")
    ax.set_xlim(lo, hi)
    ax.set_ylim(lo, hi)
    ax.set_xlabel("theoretical rate (keys/s)")
    ax.set_ylabel("SDN-dictated rate at end of run (keys/s)")
    ax.set_title(f"SDN vs theoretical rate — {len(pts_x)} commodities")
    ax.legend(loc="lower right", fontsize=8)
    fig.tight_layout()
    fig.savefig(output_path)
    plt.close(fig)
    return True


def analyze_saturation(
    log_dir: str | Path,
    buffer_size: int,
    theory_rates: dict[str, float],
    *,
    sat_threshold: float = DEFAULT_SAT_THRESHOLD,
    src_resolver: Callable[[str], str | None] | None = None,
    output_file: str | Path | None = None,
    write_csv: bool = True,
    write_plots: bool = True,
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
        write_csv: persist three CSVs under ``<log_dir>/data/`` for
            post-hoc analysis (default ``True``).
        write_plots: render four PNGs under ``<log_dir>/plots/``
            (default ``True``). Silently skipped if matplotlib is not
            installed.

    Returns:
        ``{"per_commodity": [...], "summary": {...}, "buffer_size":
        ..., "sat_threshold": ..., "csvs": [...], "plots": [...]}``.
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
    events_by_src_peer: dict[str, dict[str, list[dict[str, Any]]]] = {}
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
        rows, peer_events = _analyze_events_for_src(
            src, events, buffer_size, theory_rates, sat_threshold
        )
        per_commodity.extend(rows)
        if peer_events:
            events_by_src_peer[src] = peer_events

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

    # Global t0 across all sources/peers — used as the X-axis origin
    # for time-series CSVs and plots.
    t0: datetime | None = None
    for src_events in events_by_src_peer.values():
        for peer_events_list in src_events.values():
            for ev in peer_events_list:
                t = ev.get("t_log")
                if isinstance(t, datetime) and (t0 is None or t < t0):
                    t0 = t

    csvs_written: list[str] = []
    plots_written: list[str] = []

    if write_csv:
        data_dir = log_dir / "data"
        gen_csv = data_dir / "generator_state.csv"
        if _write_generator_state_csv(events_by_src_peer, buffer_size, t0, gen_csv):
            csvs_written.append(str(gen_csv))
        pc_csv = data_dir / "per_commodity.csv"
        if _write_per_commodity_csv(per_commodity, pc_csv):
            csvs_written.append(str(pc_csv))
        th_csv = data_dir / "theory_rates.csv"
        if _write_theory_rates_csv(theory_rates, buffer_size, th_csv):
            csvs_written.append(str(th_csv))

    if write_plots:
        plot_dir = log_dir / "plots"
        enc_plot = plot_dir / "sat_enc_over_time.png"
        if _plot_enc_over_time(
            events_by_src_peer, buffer_size, sat_threshold, t0, enc_plot
        ):
            plots_written.append(str(enc_plot))
        ratio_plot = plot_dir / "sat_ratios.png"
        if _plot_saturation_ratios(per_commodity, summary, ratio_plot):
            plots_written.append(str(ratio_plot))
        rate_plot = plot_dir / "sat_emit_rate.png"
        if _plot_emit_rate(events_by_src_peer, t0, rate_plot):
            plots_written.append(str(rate_plot))
        rate_vs_theory_plot = plot_dir / "sat_rate_vs_theory.png"
        if _plot_rate_vs_theory(per_commodity, theory_rates, rate_vs_theory_plot):
            plots_written.append(str(rate_vs_theory_plot))

    result = {
        "buffer_size": buffer_size,
        "sat_threshold": sat_threshold,
        "log_files_count": len(log_files),
        "per_commodity": per_commodity,
        "summary": summary,
        "csvs": csvs_written,
        "plots": plots_written,
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
