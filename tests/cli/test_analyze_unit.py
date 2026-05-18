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


# -----------------------------------------------------------------------------
# CSV exporters and write_csv/write_plots toggles
# -----------------------------------------------------------------------------


def _two_commodity_lines() -> list[str]:
    return [
        _state_line(
            "2026-05-17T18:00:00Z", "node-2", enc=0, emit_total=0, observed=0.0, sdn=10.0
        ),
        _state_line(
            "2026-05-17T18:00:01Z", "node-2", enc=99, emit_total=99, observed=10.0, sdn=10.0
        ),
        _state_line(
            "2026-05-17T18:00:00Z", "node-3", enc=0, emit_total=0, observed=0.0, sdn=5.0
        ),
        _state_line(
            "2026-05-17T18:00:01Z", "node-3", enc=49, emit_total=49, observed=5.0, sdn=5.0
        ),
    ]


def test_analyze_writes_three_csvs_under_data_dir() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        out = analyze_saturation(
            log_dir,
            buffer_size=100,
            theory_rates={"node-1->node-2": 100.0, "node-1->node-3": 50.0},
        )

        data_dir = log_dir / "data"
        gen = data_dir / "generator_state.csv"
        pc = data_dir / "per_commodity.csv"
        th = data_dir / "theory_rates.csv"
        assert gen.exists() and pc.exists() and th.exists()
        assert len(out["csvs"]) == 3
        # Headers
        assert gen.read_text().splitlines()[0].startswith("t_log_iso,t_seconds")
        assert pc.read_text().splitlines()[0].startswith("src,peer,commodity_id")
        assert th.read_text().splitlines()[0].startswith("commodity_id,src,peer")


def test_analyze_write_csv_false_skips_csvs() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        out = analyze_saturation(
            log_dir, 100, {"node-1->node-2": 100.0}, write_csv=False
        )
        assert out["csvs"] == []
        assert not (log_dir / "data").exists()


def test_analyze_write_plots_false_skips_plots() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        out = analyze_saturation(
            log_dir, 100, {"node-1->node-2": 100.0}, write_plots=False
        )
        assert out["plots"] == []
        assert not (log_dir / "plots").exists()


def test_analyze_writes_plots_when_matplotlib_available() -> None:
    pytest.importorskip("matplotlib")
    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        out = analyze_saturation(
            log_dir,
            100,
            {"node-1->node-2": 100.0, "node-1->node-3": 50.0},
        )
        plot_dir = log_dir / "plots"
        assert plot_dir.exists()
        names = sorted(p.name for p in plot_dir.glob("*.png"))
        assert "sat_enc_over_time.png" in names
        assert "sat_ratios.png" in names
        assert len(out["plots"]) == len(names)


# -----------------------------------------------------------------------------
# replot subcommand round-trip
# -----------------------------------------------------------------------------


def test_replot_regenerates_plots_from_csvs() -> None:
    pytest.importorskip("matplotlib")
    import argparse
    from tests.cli.dkms_topo import _run_replot

    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        analyze_saturation(
            log_dir,
            100,
            {"node-1->node-2": 100.0, "node-1->node-3": 50.0},
        )
        plot_dir = log_dir / "plots"
        for p in plot_dir.glob("*.png"):
            p.unlink()
        assert not list(plot_dir.glob("*.png"))

        ns = argparse.Namespace(
            output_dir=str(log_dir), buffer_enc_size=100, sat_threshold=0.95
        )
        rc = _run_replot(ns)
        assert rc == 0
        assert list(plot_dir.glob("*.png"))


def test_replot_missing_data_dir_returns_error() -> None:
    import argparse
    from tests.cli.dkms_topo import _run_replot

    with tempfile.TemporaryDirectory() as tmpdir:
        ns = argparse.Namespace(
            output_dir=str(Path(tmpdir) / "empty"),
            buffer_enc_size=100,
            sat_threshold=0.95,
        )
        rc = _run_replot(ns)
        assert rc == 1


def test_replot_via_cli_run_dispatches() -> None:
    pytest.importorskip("matplotlib")
    from tests.cli.dkms_topo import run

    with tempfile.TemporaryDirectory() as tmpdir:
        log_dir = Path(tmpdir)
        _write_log(log_dir, "dkms-1.log", _two_commodity_lines())
        analyze_saturation(
            log_dir,
            100,
            {"node-1->node-2": 100.0, "node-1->node-3": 50.0},
        )
        rc = run(["replot", str(log_dir)])
        assert rc == 0
