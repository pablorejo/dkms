"""Aggregate loadtest Prometheus metrics into a summary + plots.

Consumes the output of ``prom_parser.parse_metrics(...)``. Produces:

* Counts per ``status_code`` (200, 429, 5xx).
* Percentages of success / throttled / errors.
* Latency percentiles (p50, p90, p95, p99) via linear interpolation
  on the histogram buckets.
* Two PNG plots written to ``<output_dir>/plots/``:
  ``requests_by_status.png`` (bar) and ``latency_percentiles.png``
  (cumulative distribution).
* ``loadtest_analysis.json`` written to ``output_dir``.

``matplotlib`` is imported with the ``Agg`` backend so it runs
headless. Plots are skipped silently if matplotlib is unavailable.
"""

from __future__ import annotations

import json
from pathlib import Path
from typing import Any, Iterable

from tests.cli.prom_parser import filter_counter, sum_counter

REQUESTS_TOTAL = "loadtest_requests_total"
DURATION_HISTOGRAM = "loadtest_request_duration_seconds"

DEFAULT_PERCENTILES: tuple[float, ...] = (0.5, 0.9, 0.95, 0.99)


def _counts_by_status(counters: dict[str, Any]) -> dict[str, float]:
    """Return ``{status_code -> count}`` for ``loadtest_requests_total``."""
    out: dict[str, float] = {}
    for entry in counters.get(REQUESTS_TOTAL, []):
        sc = entry["labels"].get("status_code", "unknown")
        out[sc] = out.get(sc, 0.0) + entry["value"]
    return out


def _classify(status: str) -> str:
    """Map a status_code to one of ``ok / throttled / client_error /
    server_error / unknown``."""
    s = str(status)
    if s == "429":
        return "throttled"
    if s.startswith("2"):
        return "ok"
    if s.startswith("4"):
        return "client_error"
    if s.startswith("5"):
        return "server_error"
    return "unknown"


def _percentile_from_buckets(
    buckets: list[tuple[float | str, float]],
    total: float,
    p: float,
) -> float | None:
    """Linear-interpolation percentile across cumulative bucket counts.

    ``buckets`` is sorted ascending by ``le``; the last entry is the
    ``+Inf`` bucket whose count equals the total observation count.
    ``total`` is the total number of observations. ``p ∈ (0, 1]``.

    Returns the latency at the requested quantile (seconds), or
    ``None`` if there is not enough data.
    """
    if total <= 0 or not buckets:
        return None
    target = p * total
    prev_le: float | None = None
    prev_cum: float = 0.0
    for le_val, cum in buckets:
        cum_f = float(cum)
        if cum_f >= target:
            le_f: float
            if le_val == "+Inf":
                # Cannot interpolate beyond +Inf; clip to previous edge.
                return prev_le if prev_le is not None else 0.0
            le_f = float(le_val)
            if prev_le is None:
                # First non-+Inf bucket — assume linear from 0
                if cum_f <= 0:
                    return le_f
                return le_f * (target / cum_f)
            span_cum = cum_f - prev_cum
            if span_cum <= 0:
                return le_f
            span_le = le_f - prev_le
            return prev_le + (target - prev_cum) / span_cum * span_le
        if le_val != "+Inf":
            prev_le = float(le_val)
        prev_cum = cum_f
    # Target above every observed bucket → return last finite edge.
    return prev_le


def _latency_summary(
    histogram: dict[str, Any], percentiles: Iterable[float]
) -> dict[str, Any]:
    """Compute latency stats from a parsed histogram dict."""
    buckets = histogram.get("buckets", [])
    total = histogram.get("count")
    sum_v = histogram.get("sum")
    if total is None and buckets:
        total = max((float(c) for _, c in buckets), default=0.0)
    total = float(total or 0.0)
    sum_v = float(sum_v or 0.0)
    mean = sum_v / total if total > 0 else None
    pct = {}
    for p in percentiles:
        val = _percentile_from_buckets(buckets, total, p)
        pct[f"p{int(round(p * 100))}"] = val
    return {
        "count": total,
        "sum_seconds": sum_v,
        "mean_seconds": mean,
        "percentiles_seconds": pct,
    }


def _plot_requests_by_status(
    counts: dict[str, float], output_path: Path
) -> bool:
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore
    except Exception:
        return False
    if not counts:
        return False
    statuses = sorted(counts.keys())
    values = [counts[s] for s in statuses]
    fig, ax = plt.subplots(figsize=(6, 4))
    ax.bar(statuses, values, color=["#2ecc71" if s.startswith("2") else "#e67e22" if s == "429" else "#e74c3c" for s in statuses])
    ax.set_xlabel("HTTP status code")
    ax.set_ylabel("requests")
    ax.set_title("loadtest requests by status code")
    for i, v in enumerate(values):
        ax.text(i, v, f"{int(v)}", ha="center", va="bottom", fontsize=9)
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _plot_latency_cdf(
    buckets: list[tuple[float | str, float]],
    total: float,
    percentiles_seconds: dict[str, Any],
    output_path: Path,
) -> bool:
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore
    except Exception:
        return False
    if not buckets or total <= 0:
        return False
    # Drop the +Inf bucket for the X axis
    finite = [(float(le), float(c) / total) for le, c in buckets if le != "+Inf"]
    if not finite:
        return False
    finite.sort(key=lambda b: b[0])
    xs = [b[0] for b in finite]
    ys = [b[1] for b in finite]
    fig, ax = plt.subplots(figsize=(7, 4))
    ax.step(xs, ys, where="post", color="#2980b9", linewidth=1.5)
    ax.set_xlabel("latency (seconds)")
    ax.set_ylabel("cumulative fraction of requests")
    ax.set_title("loadtest request latency CDF")
    ax.set_xscale("log")
    ax.set_ylim(0, 1.05)
    for name, val in percentiles_seconds.items():
        if isinstance(val, (int, float)):
            ax.axvline(val, color="#c0392b", linestyle="--", linewidth=0.8)
            ax.text(val, 1.0, f" {name}={val:.4g}", color="#c0392b", fontsize=8, va="top")
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path)
    plt.close(fig)
    return True


def analyze_loadtest(
    metrics: dict[str, Any],
    params: dict[str, Any] | None = None,
    *,
    output_dir: str | Path | None = None,
    percentiles: Iterable[float] = DEFAULT_PERCENTILES,
    write_plots: bool = True,
) -> dict[str, Any]:
    """Summarize a parsed Prometheus metrics dict.

    Args:
        metrics: Output of ``prom_parser.parse_metrics(text)``.
        params: Original loadtest params (echoed in the JSON output for
            traceability). May be ``None``.
        output_dir: Directory where ``loadtest_analysis.json`` and
            ``plots/`` are written. If ``None``, nothing is persisted
            and only the dict is returned.
        percentiles: Quantiles in (0, 1] (default 50/90/95/99).
        write_plots: Skip plots if False (or if matplotlib missing).

    Returns:
        ``{"status_counts", "status_classes", "percentages", "totals",
        "latency", "params", "plots"}``.
    """
    counters = metrics.get("counters", {})
    histograms = metrics.get("histograms", {})

    status_counts = _counts_by_status(counters)
    total = sum(status_counts.values())
    classes: dict[str, float] = {}
    for status, count in status_counts.items():
        cls = _classify(status)
        classes[cls] = classes.get(cls, 0.0) + count

    percentages = (
        {k: (v / total) * 100.0 for k, v in classes.items()} if total > 0 else {}
    )

    histogram = histograms.get(DURATION_HISTOGRAM, {})
    latency = _latency_summary(histogram, percentiles)

    result: dict[str, Any] = {
        "totals": {
            "requests": total,
            "by_status": status_counts,
        },
        "status_classes": classes,
        "percentages": percentages,
        "latency": latency,
        "params": dict(params) if params is not None else None,
        "plots": [],
    }

    if output_dir is not None:
        out_dir = Path(output_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        if write_plots:
            plot_dir = out_dir / "plots"
            req_plot = plot_dir / "requests_by_status.png"
            lat_plot = plot_dir / "latency_percentiles.png"
            if _plot_requests_by_status(status_counts, req_plot):
                result["plots"].append(str(req_plot))
            if _plot_latency_cdf(
                histogram.get("buckets", []),
                latency["count"],
                latency["percentiles_seconds"],
                lat_plot,
            ):
                result["plots"].append(str(lat_plot))
        with open(out_dir / "loadtest_analysis.json", "w", encoding="utf-8") as fh:
            json.dump(result, fh, indent=2, default=str)

    return result


# Convenience re-exports for tests / callers.
__all__ = [
    "DEFAULT_PERCENTILES",
    "DURATION_HISTOGRAM",
    "REQUESTS_TOTAL",
    "analyze_loadtest",
]
