"""Unit tests for ``tests/cli/loadtest_analyze``."""

from __future__ import annotations

import csv
import tempfile
from pathlib import Path

import pytest

from tests.cli.loadtest_analyze import (
    DURATION_HISTOGRAM,
    analyze_loadtest,
)


def _metrics_two_statuses() -> dict[str, dict]:
    """Construct a small ``parse_metrics``-shaped dict by hand."""
    return {
        "counters": {
            "loadtest_requests_total": [
                {"labels": {"status_code": "200", "result": "ok"}, "value": 90.0},
                {"labels": {"status_code": "429", "result": "throttled"}, "value": 10.0},
            ],
            "loadtest_errors_total": [
                {"labels": {"reason": "throttled"}, "value": 10.0},
            ],
        },
        "histograms": {
            DURATION_HISTOGRAM: {
                "buckets": [
                    (0.005, 50.0),
                    (0.01, 80.0),
                    (0.05, 95.0),
                    (0.1, 100.0),
                    (float("inf"), 100.0),
                ],
                "count": 100.0,
                "sum": 1.5,
            },
        },
    }


def test_analyze_loadtest_returns_status_counts() -> None:
    out = analyze_loadtest(_metrics_two_statuses(), {})
    assert out["totals"]["requests"] == 100
    assert out["totals"]["by_status"]["200"] == 90.0
    assert out["totals"]["by_status"]["429"] == 10.0
    # _classify maps "200"->"ok" and "429"->"throttled"
    assert out["status_classes"]["ok"] == 90.0
    assert out["status_classes"]["throttled"] == 10.0
    assert out["percentages"]["ok"] == pytest.approx(90.0)


def test_analyze_loadtest_latency_percentiles() -> None:
    out = analyze_loadtest(_metrics_two_statuses(), {})
    lat = out["latency"]
    assert lat["count"] == 100.0
    # Percentile keys are "p50", "p90", "p95", "p99" (str), not floats.
    # Cumulative 50 at le=0.005 → p50 should resolve to 0.005;
    # cumulative 95 at le=0.05 → p95 should resolve to 0.05.
    assert lat["percentiles_seconds"]["p50"] == pytest.approx(0.005)
    assert lat["percentiles_seconds"]["p95"] == pytest.approx(0.05)


def test_analyze_loadtest_writes_csv_when_output_dir_set(tmp_path: Path) -> None:
    out = analyze_loadtest(_metrics_two_statuses(), {}, output_dir=tmp_path)
    assert (tmp_path / "data" / "loadtest_metrics.csv").exists()
    assert any("loadtest_metrics.csv" in c for c in out["csvs"])


def test_analyze_loadtest_write_csv_false_skips_csv(tmp_path: Path) -> None:
    out = analyze_loadtest(
        _metrics_two_statuses(), {}, output_dir=tmp_path, write_csv=False
    )
    assert out["csvs"] == []
    assert not (tmp_path / "data").exists()


def test_analyze_loadtest_write_plots_false_skips_plots(tmp_path: Path) -> None:
    out = analyze_loadtest(
        _metrics_two_statuses(), {}, output_dir=tmp_path, write_plots=False
    )
    assert out["plots"] == []
    assert not (tmp_path / "plots").exists()


def test_analyze_loadtest_writes_plots_when_matplotlib_available(
    tmp_path: Path,
) -> None:
    pytest.importorskip("matplotlib")
    out = analyze_loadtest(_metrics_two_statuses(), {}, output_dir=tmp_path)
    plot_dir = tmp_path / "plots"
    assert plot_dir.exists()
    names = sorted(p.name for p in plot_dir.glob("*.png"))
    # 3 plots are always producible from this dataset:
    # requests_by_status, latency_percentiles, sae_errors_by_reason.
    assert "requests_by_status.png" in names
    assert "latency_percentiles.png" in names
    assert "sae_errors_by_reason.png" in names
    assert len(out["plots"]) == len(names)


def test_analyze_loadtest_time_series_plots_when_requests_csv_given(
    tmp_path: Path,
) -> None:
    pytest.importorskip("matplotlib")
    rcsv = tmp_path / "requests.csv"
    with open(rcsv, "w", newline="", encoding="utf-8") as fh:
        w = csv.writer(fh)
        w.writerow(
            ["emitted_at_epoch", "elapsed_seconds", "status_code"]
        )
        for i in range(20):
            w.writerow([1747500000.0 + i, 0.012, 200])
        for i in range(20, 30):
            w.writerow([1747500000.0 + i, 0.080, 429])
    out = analyze_loadtest(
        _metrics_two_statuses(),
        {},
        output_dir=tmp_path,
        requests_csv=rcsv,
    )
    names = sorted(p.name for p in (tmp_path / "plots").glob("*.png"))
    assert "sae_status_over_time.png" in names
    assert "sae_latency_p95_over_time.png" in names
