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

import csv
import json
from pathlib import Path
from typing import Any, Iterable

from tests.cli.prom_parser import filter_counter, sum_counter

REQUESTS_TOTAL = "loadtest_requests_total"
DURATION_HISTOGRAM = "loadtest_request_duration_seconds"

DEFAULT_PERCENTILES: tuple[float, ...] = (0.5, 0.9, 0.95, 0.99)

# Time-series window for "over-time" plots derived from requests.csv.
_WINDOW_SECONDS_DEFAULT: float = 5.0


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


def _plot_errors_by_reason(
    counters: dict[str, Any], output_path: Path
) -> bool:
    """Bar chart of ``loadtest_errors_total`` grouped by ``reason``."""
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore
    except Exception:
        return False
    entries = counters.get("loadtest_errors_total", [])
    if not entries:
        return False
    reasons: dict[str, float] = {}
    for e in entries:
        reason = e["labels"].get("reason", "unknown")
        reasons[reason] = reasons.get(reason, 0.0) + e["value"]
    if not reasons:
        return False
    items = sorted(reasons.items(), key=lambda kv: -kv[1])
    labels = [r for r, _ in items]
    values = [v for _, v in items]
    fig, ax = plt.subplots(figsize=(7, max(3, 0.4 * len(items))))
    color_for = {
        "ok": "#2ecc71",
        "http_429": "#e67e22",
        "429_other": "#e67e22",
        "http_503": "#9b59b6",
        "503_other": "#9b59b6",
    }
    bars = ax.barh(
        range(len(items)),
        values,
        color=[color_for.get(r, "#e74c3c") for r in labels],
    )
    ax.set_yticks(range(len(items)))
    ax.set_yticklabels(labels, fontsize=9)
    ax.invert_yaxis()
    ax.set_xlabel("requests")
    ax.set_title("loadtest errors by reason")
    for i, v in enumerate(values):
        ax.text(v, i, f" {int(v)}", va="center", fontsize=8)
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _read_requests_csv(csv_path: Path) -> list[dict[str, Any]]:
    """Parse the per-request CSV emitted by the loadtest runner.

    Columns we use: ``emitted_at_epoch`` (float), ``elapsed_seconds``
    (float), ``status_code`` (int). Unparseable rows are skipped.
    """
    rows: list[dict[str, Any]] = []
    if not csv_path.exists():
        return rows
    try:
        with open(csv_path, "r", encoding="utf-8", errors="replace") as fh:
            reader = csv.DictReader(fh)
            for raw in reader:
                try:
                    emitted = float(raw.get("emitted_at_epoch") or 0.0)
                    elapsed = float(raw.get("elapsed_seconds") or 0.0)
                    sc = int(raw.get("status_code") or 0)
                except (TypeError, ValueError):
                    continue
                if emitted <= 0:
                    continue
                rows.append({"emitted": emitted, "elapsed": elapsed, "status": sc})
    except Exception:  # noqa: BLE001
        return []
    return rows


def _window_buckets(
    requests: list[dict[str, Any]], window_s: float
) -> list[dict[str, Any]]:
    """Group requests by floor((emitted - t0) / window_s).

    Returns one dict per bucket with: t_center (s since t0), n,
    by_status, latencies (sorted), p95.
    """
    if not requests:
        return []
    t0 = min(r["emitted"] for r in requests)
    bucket_map: dict[int, dict[str, Any]] = {}
    for r in requests:
        rel = r["emitted"] - t0
        idx = int(rel // window_s)
        bucket = bucket_map.setdefault(
            idx,
            {"idx": idx, "n": 0, "by_status": {}, "latencies": []},
        )
        bucket["n"] += 1
        sc = r["status"]
        bucket["by_status"][sc] = bucket["by_status"].get(sc, 0) + 1
        bucket["latencies"].append(r["elapsed"])
    out: list[dict[str, Any]] = []
    for idx in sorted(bucket_map.keys()):
        b = bucket_map[idx]
        lat = sorted(b["latencies"])
        b["t_center"] = (idx + 0.5) * window_s
        if lat:
            p95_idx = max(0, int(0.95 * (len(lat) - 1)))
            b["p95"] = lat[p95_idx]
        else:
            b["p95"] = 0.0
        out.append(b)
    return out


def _plot_status_over_time(
    requests_csv: Path, output_path: Path, window_s: float = _WINDOW_SECONDS_DEFAULT
) -> bool:
    """Stacked area of % requests per status code over time windows."""
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore
    except Exception:
        return False
    rows = _read_requests_csv(requests_csv)
    buckets = _window_buckets(rows, window_s)
    if not buckets:
        return False
    # Collect the union of status codes; classify into groups for color stability.
    groups: list[tuple[str, list[int], str]] = [
        ("2xx", list(range(200, 300)), "#2ecc71"),
        ("429", [429], "#e67e22"),
        ("503", [503], "#9b59b6"),
        ("4xx other", [c for c in range(400, 500) if c != 429], "#f1c40f"),
        ("5xx other", [c for c in range(500, 600) if c != 503], "#e74c3c"),
        ("conn err", [0], "#7f8c8d"),
    ]
    xs = [b["t_center"] for b in buckets]
    series: list[tuple[str, str, list[float]]] = []
    for name, codes, color in groups:
        ys: list[float] = []
        for b in buckets:
            n_in_group = sum(b["by_status"].get(c, 0) for c in codes)
            pct = (n_in_group / b["n"] * 100.0) if b["n"] > 0 else 0.0
            ys.append(pct)
        if any(y > 0 for y in ys):
            series.append((name, color, ys))
    if not series:
        return False
    fig, ax = plt.subplots(figsize=(10, 5))
    ax.stackplot(
        xs,
        *[s[2] for s in series],
        labels=[s[0] for s in series],
        colors=[s[1] for s in series],
        alpha=0.85,
    )
    ax.set_xlabel(f"seconds since first request (window={int(window_s)}s)")
    ax.set_ylabel("% of requests in window")
    ax.set_ylim(0, 100)
    ax.set_title("status code distribution over time")
    ax.legend(loc="lower right", fontsize=8, ncol=2)
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _plot_latency_p95_over_time(
    requests_csv: Path,
    output_path: Path,
    window_s: float = _WINDOW_SECONDS_DEFAULT,
) -> bool:
    """Line of p95 latency per window. Overlays median and request rate."""
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore
    except Exception:
        return False
    rows = _read_requests_csv(requests_csv)
    buckets = _window_buckets(rows, window_s)
    if not buckets:
        return False
    xs = [b["t_center"] for b in buckets]
    p95 = [b["p95"] * 1000.0 for b in buckets]  # ms
    rate = [b["n"] / window_s for b in buckets]
    fig, ax1 = plt.subplots(figsize=(10, 5))
    ax1.plot(xs, p95, color="#c0392b", linewidth=1.5, label="p95 latency (ms)")
    ax1.set_xlabel(f"seconds since first request (window={int(window_s)}s)")
    ax1.set_ylabel("p95 latency (ms)", color="#c0392b")
    ax1.tick_params(axis="y", labelcolor="#c0392b")
    ax1.set_ylim(bottom=0)
    ax2 = ax1.twinx()
    ax2.plot(xs, rate, color="#2980b9", linewidth=1.0, linestyle="--", label="req/s")
    ax2.set_ylabel("request rate (req/s)", color="#2980b9")
    ax2.tick_params(axis="y", labelcolor="#2980b9")
    ax2.set_ylim(bottom=0)
    ax1.set_title("p95 latency over time vs request rate")
    lines, labels = ax1.get_legend_handles_labels()
    lines2, labels2 = ax2.get_legend_handles_labels()
    ax1.legend(lines + lines2, labels + labels2, loc="upper left", fontsize=8)
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path)
    plt.close(fig)
    return True


def _write_loadtest_metrics_csv(
    metrics: dict[str, Any], output_path: Path
) -> bool:
    """Flatten counters + histogram-summary to a single tabular CSV.

    Rows: one per (metric_name, status_code, result/reason) for
    counters, plus one row per histogram bucket. Useful for diffing
    runs at a glance without `prom_parser.parse_metrics`.
    """
    counters = metrics.get("counters", {})
    histograms = metrics.get("histograms", {})
    if not counters and not histograms:
        return False
    output_path.parent.mkdir(parents=True, exist_ok=True)
    with open(output_path, "w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh)
        w.writerow(["metric", "label_key", "label_value", "value"])
        for name in sorted(counters.keys()):
            for entry in counters[name]:
                if entry["labels"]:
                    for k in sorted(entry["labels"].keys()):
                        w.writerow([name, k, entry["labels"][k], entry["value"]])
                    # Also write a summary row keyed by combined labels.
                    combined = "|".join(
                        f"{k}={entry['labels'][k]}" for k in sorted(entry["labels"].keys())
                    )
                    w.writerow([name, "_all_labels", combined, entry["value"]])
                else:
                    w.writerow([name, "", "", entry["value"]])
        for base in sorted(histograms.keys()):
            h = histograms[base]
            for le, count in h.get("buckets", []):
                w.writerow([f"{base}_bucket", "le", str(le), count])
            if h.get("count") is not None:
                w.writerow([f"{base}_count", "", "", h["count"]])
            if h.get("sum") is not None:
                w.writerow([f"{base}_sum", "", "", h["sum"]])
    return True


def analyze_loadtest(
    metrics: dict[str, Any],
    params: dict[str, Any] | None = None,
    *,
    output_dir: str | Path | None = None,
    percentiles: Iterable[float] = DEFAULT_PERCENTILES,
    write_plots: bool = True,
    write_csv: bool = True,
    requests_csv: Path | None = None,
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
        "csvs": [],
    }

    if output_dir is not None:
        out_dir = Path(output_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        if write_csv:
            data_dir = out_dir / "data"
            metrics_csv = data_dir / "loadtest_metrics.csv"
            if _write_loadtest_metrics_csv(metrics, metrics_csv):
                result["csvs"].append(str(metrics_csv))
        if write_plots:
            plot_dir = out_dir / "plots"
            req_plot = plot_dir / "requests_by_status.png"
            lat_plot = plot_dir / "latency_percentiles.png"
            err_plot = plot_dir / "sae_errors_by_reason.png"
            status_time_plot = plot_dir / "sae_status_over_time.png"
            p95_time_plot = plot_dir / "sae_latency_p95_over_time.png"
            if _plot_requests_by_status(status_counts, req_plot):
                result["plots"].append(str(req_plot))
            if _plot_latency_cdf(
                histogram.get("buckets", []),
                latency["count"],
                latency["percentiles_seconds"],
                lat_plot,
            ):
                result["plots"].append(str(lat_plot))
            if _plot_errors_by_reason(metrics.get("counters", {}), err_plot):
                result["plots"].append(str(err_plot))
            # Time-series plots need the per-request CSV — only fired
            # when the caller passed its path.
            if requests_csv is not None:
                rcsv = Path(requests_csv)
                if _plot_status_over_time(rcsv, status_time_plot):
                    result["plots"].append(str(status_time_plot))
                if _plot_latency_p95_over_time(rcsv, p95_time_plot):
                    result["plots"].append(str(p95_time_plot))
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
