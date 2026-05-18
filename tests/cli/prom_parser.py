"""Parse Prometheus text-format metrics emitted by the loadtest pod.

Loadtest exposes (see ``orchestrator/...loadtest``):

* ``loadtest_requests_total{status_code,result}`` — counter.
* ``loadtest_request_duration_seconds_{bucket,count,sum}`` — histogram.

The Prometheus text format is fully documented at
https://prometheus.io/docs/instrumenting/exposition_formats/, but we
only need the subset above. No external dep (no ``prometheus_client``).

Output of :func:`parse_metrics`::

    {
        "counters": {
            "loadtest_requests_total": [
                {"labels": {"status_code": "200", "result": "success"},
                 "value": 100.0},
                ...
            ],
        },
        "histograms": {
            "loadtest_request_duration_seconds": {
                "buckets": [(0.005, 100), (0.01, 105), ..., ("+Inf", 110)],
                "count": 110,
                "sum": 0.123,
            },
        },
        "unknown": {  # any metric we don't classify
            "raw_metric_name": [{"labels": {...}, "value": ...}, ...]
        },
    }

The ``unknown`` bucket keeps anything that doesn't match the
loadtest counter/histogram shape, so the caller can spot unexpected
metrics without losing data.
"""

from __future__ import annotations

import re
from typing import Any

# Pattern: <metric>{<labels>} <value> [timestamp]
# Labels are comma-separated `name="quoted-value"`.
_LINE_RE = re.compile(
    r"""
    ^(?P<name>[A-Za-z_:][A-Za-z0-9_:]*)
    (?:\{(?P<labels>[^}]*)\})?
    \s+(?P<value>[^\s]+)
    (?:\s+(?P<timestamp>\d+))?\s*$
    """,
    re.VERBOSE,
)

_LABEL_RE = re.compile(r'(\w+)="((?:\\"|[^"])*)"')

# Numeric values may include nan/inf per Prometheus spec.
_SPECIAL_VALUES = {"NaN": float("nan"), "+Inf": float("inf"), "-Inf": float("-inf")}


def _parse_value(raw: str) -> float | None:
    if raw in _SPECIAL_VALUES:
        return _SPECIAL_VALUES[raw]
    try:
        return float(raw)
    except ValueError:
        return None


def _parse_labels(raw: str | None) -> dict[str, str]:
    if not raw:
        return {}
    out: dict[str, str] = {}
    for m in _LABEL_RE.finditer(raw):
        key = m.group(1)
        value = m.group(2).replace("\\\\", "\\").replace('\\"', '"').replace("\\n", "\n")
        out[key] = value
    return out


def _parse_le(label_value: str) -> float | str:
    if label_value in ("+Inf", "Inf"):
        return "+Inf"
    try:
        return float(label_value)
    except ValueError:
        return label_value


def parse_metrics(text: str) -> dict[str, Any]:
    """Parse a Prometheus text-format payload into a structured dict.

    Comments (``# HELP``, ``# TYPE``) and blank lines are skipped.
    Unparseable lines are silently ignored.
    """
    counters: dict[str, list[dict[str, Any]]] = {}
    histograms: dict[str, dict[str, Any]] = {}
    unknown: dict[str, list[dict[str, Any]]] = {}

    for raw_line in text.splitlines():
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        m = _LINE_RE.match(line)
        if not m:
            continue
        name = m.group("name")
        labels = _parse_labels(m.group("labels"))
        value = _parse_value(m.group("value"))
        if value is None:
            continue

        # Histogram detection: `<base>_bucket{le=...}` / `<base>_count` / `<base>_sum`.
        if name.endswith("_bucket") and "le" in labels:
            base = name[: -len("_bucket")]
            hist = histograms.setdefault(
                base, {"buckets": [], "count": None, "sum": None, "labels_extra": {}}
            )
            le = _parse_le(labels["le"])
            hist["buckets"].append((le, value))
            # Capture extra labels (other than 'le') from the first bucket only.
            if not hist["labels_extra"]:
                extra = {k: v for k, v in labels.items() if k != "le"}
                if extra:
                    hist["labels_extra"] = extra
            continue
        if name.endswith("_count"):
            base = name[: -len("_count")]
            # Heuristic: only treat as histogram if we've seen its buckets;
            # otherwise it's a plain counter named e.g. `something_count`.
            if base in histograms:
                histograms[base]["count"] = int(value)
                continue
        if name.endswith("_sum"):
            base = name[: -len("_sum")]
            if base in histograms:
                histograms[base]["sum"] = value
                continue

        # Treat anything else as a labelled counter / gauge.
        bucket = counters if name.endswith("_total") else unknown
        bucket.setdefault(name, []).append({"labels": labels, "value": value})

    # Normalize histograms: sort buckets by `le` (with +Inf last).
    for base, hist in histograms.items():
        hist["buckets"].sort(
            key=lambda b: (float("inf") if b[0] == "+Inf" else float(b[0]))
        )

    return {
        "counters": counters,
        "histograms": histograms,
        "unknown": unknown,
    }


def sum_counter(counters: dict[str, list[dict[str, Any]]], name: str) -> float:
    """Convenience: sum all label-combinations of a counter."""
    entries = counters.get(name, [])
    return sum(e["value"] for e in entries)


def filter_counter(
    counters: dict[str, list[dict[str, Any]]],
    name: str,
    **label_match: str,
) -> float:
    """Sum entries of ``name`` whose labels match every key=value in
    ``label_match`` (subset match)."""
    total = 0.0
    for e in counters.get(name, []):
        labels = e["labels"]
        if all(labels.get(k) == v for k, v in label_match.items()):
            total += e["value"]
    return total


__all__ = [
    "filter_counter",
    "parse_metrics",
    "sum_counter",
]
