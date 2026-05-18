"""Unit tests for ``tests/cli/analyze``."""

from __future__ import annotations

import json
import tempfile
from pathlib import Path

import pytest

from tests.cli.analyze import (
    DEFAULT_SAT_THRESHOLD,
    _default_src_resolver,
    analyze_saturation,
)


# -----------------------------------------------------------------------------
# Helpers
# -----------------------------------------------------------------------------


def _state_line(
    t: str,
    peer: str,
    enc: int = 0,
    emit_total: int = 0,
    observed: float = 0.0,
    sdn: float = 0.0,
    dec: int = 0,
    ack: int = 0,
) -> str:
    return (
        f"{t} INFO generator.state peer={peer} enc={enc} dec={dec} "
        f"ack_pending={ack} emit_total={emit_total} "
        f'observed_keys_per_s="{observed}" sdn_rate_keys_per_s="{sdn}"'
    )


def _write_log(log_dir: Path, name: str, lines: list[str]) -> Path:
    p = log_dir / name
    p.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return p


# -----------------------------------------------------------------------------
# Source resolver
# -----------------------------------------------------------------------------


def test_resolver_bare_node_uid() -> None:
    assert _default_src_resolver("node-5.log") == "node-5"


def test_resolver_kubernetes_pod_name() -> None:
    assert _default_src_resolver("dkms-30-9d87758d8-phswx.log") == "node-30"


def test_resolver_simple_dkms_name() -> None:
    assert _default_src_resolver("dkms-3.log") == "node-3"


def test_resolver_no_digits_returns_none() -> None:
    assert _default_src_resolver("nothing.log") is None


def test_resolver_strips_log_suffix_only() -> None:
    assert _default_src_resolver("dkms-7.bak.log") == "node-7"


# -----------------------------------------------------------------------------
# analyze_saturation: validation
# -----------------------------------------------------------------------------


def test_analyze_raises_when_log_dir_missing() -> None:
    with pytest.raises(FileNotFoundError):
        analyze_saturation("/no/such/path/at/all", 100, {})


def test_analyze_rejects_zero_buffer() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        with pytest.raises(ValueError):
            analyze_saturation(tmpdir, 0, {})


def test_analyze_rejects_threshold_out_of_range() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        with pytest.raises(ValueError):
            analyze_saturation(tmpdir, 100, {}, sat_threshold=0.0)
        with pytest.raises(ValueError):
            analyze_saturation(tmpdir, 100, {}, sat_threshold=1.5)


# -----------------------------------------------------------------------------
# analyze_saturation: empty inputs
# -----------------------------------------------------------------------------


def test_analyze_empty_directory_returns_empty_result() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        out = analyze_saturation(tmpdir, 100, {})
    assert out["per_commodity"] == []
    assert out["summary"]["saturated_count"] == 0
    assert out["summary"]["total_count"] == 0
    assert out["summary"]["median_ratio"] is None
    assert out["log_files_count"] == 0


def test_analyze_ignores_logs_with_unresolvable_filenames() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "nothing.log",  # default resolver returns None
            [_state_line("2026-05-17T18:00:00Z", "node-2", enc=10, emit_total=10)],
        )
        out = analyze_saturation(log_dir, 100, {})
    assert out["per_commodity"] == []


# -----------------------------------------------------------------------------
# analyze_saturation: happy paths
# -----------------------------------------------------------------------------


def test_analyze_two_commodities_correct_observed_times() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        # src=node-1 emits to node-2 (saturates at +10s) and node-3 (+20s)
        # Buffer=100, sat_threshold default 0.95 -> target 95
        lines = [
            _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
            _state_line("2026-05-17T18:00:01Z", "node-2", enc=10, emit_total=10),
            _state_line("2026-05-17T18:00:11Z", "node-2", enc=95, emit_total=95),
            _state_line("2026-05-17T18:00:00Z", "node-3", enc=0, emit_total=0),
            _state_line("2026-05-17T18:00:01Z", "node-3", enc=5, emit_total=5),
            _state_line("2026-05-17T18:00:21Z", "node-3", enc=95, emit_total=100),
        ]
        _write_log(log_dir, "dkms-1.log", lines)
        out = analyze_saturation(
            log_dir,
            buffer_size=100,
            theory_rates={"node-1->node-2": 10.0, "node-1->node-3": 5.0},
        )

    rows = {r["commodity_id"]: r for r in out["per_commodity"]}
    r2 = rows["node-1->node-2"]
    assert r2["saturated"] is True
    assert r2["t_observed_seconds"] == pytest.approx(10.0)
    assert r2["t_theoretical_seconds"] == pytest.approx(10.0)
    assert r2["ratio"] == pytest.approx(1.0)
    assert r2["enc_at_sat"] == 95

    r3 = rows["node-1->node-3"]
    assert r3["saturated"] is True
    assert r3["t_observed_seconds"] == pytest.approx(20.0)
    assert r3["ratio"] == pytest.approx(1.0)


def test_analyze_unsaturated_commodity_has_null_ratio() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "dkms-1.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line(
                    "2026-05-17T18:00:05Z", "node-2", enc=20, emit_total=20
                ),
            ],
        )
        out = analyze_saturation(
            log_dir, 100, {"node-1->node-2": 5.0}
        )
    row = out["per_commodity"][0]
    assert row["saturated"] is False
    assert row["ratio"] is None
    assert row["t_observed_seconds"] is None
    assert row["max_enc"] == 20


def test_analyze_missing_theory_rate_yields_null_ratio() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "dkms-1.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line("2026-05-17T18:00:01Z", "node-2", enc=10, emit_total=10),
                _state_line(
                    "2026-05-17T18:00:11Z", "node-2", enc=99, emit_total=99
                ),
            ],
        )
        # No theory_rates passed
        out = analyze_saturation(log_dir, 100, {})
    row = out["per_commodity"][0]
    assert row["saturated"] is True
    assert row["t_observed_seconds"] == pytest.approx(10.0)
    assert row["t_theoretical_seconds"] is None
    assert row["ratio"] is None


def test_analyze_writes_sat_analysis_json() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "dkms-1.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line(
                    "2026-05-17T18:00:01Z", "node-2", enc=99, emit_total=99
                ),
            ],
        )
        analyze_saturation(log_dir, 100, {"node-1->node-2": 100.0})
        sat_json = log_dir / "sat_analysis.json"
        assert sat_json.exists()
        parsed = json.loads(sat_json.read_text())
        assert parsed["buffer_size"] == 100
        assert parsed["sat_threshold"] == DEFAULT_SAT_THRESHOLD
        assert parsed["summary"]["saturated_count"] == 1


def test_analyze_custom_output_file_path() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "dkms-1.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line(
                    "2026-05-17T18:00:01Z", "node-2", enc=99, emit_total=99
                ),
            ],
        )
        custom = log_dir / "subdir" / "analysis.json"
        analyze_saturation(
            log_dir, 100, {"node-1->node-2": 100.0}, output_file=custom
        )
        assert custom.exists()
        # default file should NOT have been created when output_file is set
        assert not (log_dir / "sat_analysis.json").exists()


def test_analyze_custom_resolver_is_honored() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "weird-name.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line(
                    "2026-05-17T18:00:01Z", "node-2", enc=99, emit_total=99
                ),
            ],
        )
        out = analyze_saturation(
            log_dir,
            100,
            {"custom-src->node-2": 100.0},
            src_resolver=lambda fn: "custom-src",
        )
    assert out["per_commodity"][0]["src"] == "custom-src"


def test_analyze_summary_quartiles_on_single_sample() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(
            log_dir,
            "dkms-1.log",
            [
                _state_line("2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0),
                _state_line(
                    "2026-05-17T18:00:01Z", "node-2", enc=99, emit_total=99
                ),
            ],
        )
        out = analyze_saturation(log_dir, 100, {"node-1->node-2": 50.0})
    s = out["summary"]
    assert s["ratio_count"] == 1
    # With one value, p25 == p75 == median
    assert s["median_ratio"] == s["p25_ratio"] == s["p75_ratio"]
