"""Unit tests for ``tests/cli/bench_multipath``."""

from __future__ import annotations

import csv
import tempfile
from pathlib import Path

import pytest

from tests.cli.bench_multipath import (
    DEFAULT_BUFFER_SIZE,
    DEFAULT_STARVATION_THRESHOLD,
    compute_metrics,
    verify_against_baseline,
)


def _write_per_commodity(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    cols = [
        "src",
        "peer",
        "commodity_id",
        "samples",
        "max_enc",
        "enc_at_sat",
        "t_first_emit",
        "t_saturated",
        "t_observed_seconds",
        "t_theoretical_seconds",
        "ratio",
        "saturated",
        "theory_rate_keys_per_s",
        "last_sdn_rate_keys_per_s",
        "last_observed_keys_per_s",
    ]
    with open(path, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=cols, extrasaction="ignore")
        w.writeheader()
        for r in rows:
            w.writerow({c: r.get(c, "") for c in cols})


def _write_generator_state(path: Path, rows: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    cols = [
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
    with open(path, "w", newline="", encoding="utf-8") as fh:
        w = csv.DictWriter(fh, fieldnames=cols, extrasaction="ignore")
        w.writeheader()
        for r in rows:
            w.writerow({c: r.get(c, "") for c in cols})


def test_compute_metrics_minimal_two_commodities() -> None:
    """Run sintético con 2 commodities — uno saturado, uno parcial."""
    with tempfile.TemporaryDirectory() as tmpdir:
        run_dir = Path(tmpdir)
        _write_per_commodity(
            run_dir / "data" / "per_commodity.csv",
            [
                {
                    "src": "dkms-1",
                    "peer": "dkms-2",
                    "commodity_id": "dkms-1->dkms-2",
                    "max_enc": 62500,
                    "t_observed_seconds": 200.0,
                    "saturated": "True",
                },
                {
                    "src": "dkms-1",
                    "peer": "dkms-3",
                    "commodity_id": "dkms-1->dkms-3",
                    "max_enc": 30000,
                    "saturated": "False",
                },
            ],
        )
        _write_generator_state(
            run_dir / "data" / "generator_state.csv",
            [
                # dkms-1→dkms-2: llena hasta 62500 (saturado al 95%)
                {
                    "t_seconds": 100.0,
                    "src": "dkms-1",
                    "peer": "dkms-2",
                    "commodity_id": "dkms-1->dkms-2",
                    "enc": 30000,
                    "emit_total": 30000,
                },
                {
                    "t_seconds": 500.0,
                    "src": "dkms-1",
                    "peer": "dkms-2",
                    "commodity_id": "dkms-1->dkms-2",
                    "enc": 62500,
                    "emit_total": 62500,
                },
                # dkms-1→dkms-3: rate baja, llega solo a 30000
                {
                    "t_seconds": 100.0,
                    "src": "dkms-1",
                    "peer": "dkms-3",
                    "commodity_id": "dkms-1->dkms-3",
                    "enc": 10000,
                    "emit_total": 10000,
                },
                {
                    "t_seconds": 500.0,
                    "src": "dkms-1",
                    "peer": "dkms-3",
                    "commodity_id": "dkms-1->dkms-3",
                    "enc": 30000,
                    "emit_total": 30000,
                },
            ],
        )
        m = compute_metrics(run_dir, run_duration=600.0)

        assert m.n_commodities == 2
        assert m.n_saturated == 1
        # Fills: 62500/65536≈0.954, 30000/65536≈0.458.
        # Spread = 0.954 - 0.458 ≈ 0.496.
        assert 0.45 < m.spread_fill_ratio < 0.55
        # Min fill ≈ 0.458.
        assert 0.45 < m.min_fill_ratio < 0.47
        # Production: 62500 + 30000 = 92500.
        assert m.total_production_keys == 92500
        # Saturation aggregate: (600 - 200) = 400s para el saturado.
        assert abs(m.saturation_time_seconds - 400.0) < 0.1


def test_compute_metrics_detects_starvation_continuous() -> None:
    """Un DKMS con buffer continuamente bajo 0.15 marca starvation."""
    with tempfile.TemporaryDirectory() as tmpdir:
        run_dir = Path(tmpdir)
        _write_per_commodity(
            run_dir / "data" / "per_commodity.csv",
            [
                {
                    "src": "dkms-1",
                    "peer": "dkms-2",
                    "commodity_id": "dkms-1->dkms-2",
                    "max_enc": 5000,
                    "saturated": "False",
                },
            ],
        )
        # 600 s a fill ~0.08 (< 0.15) — 120 buckets de 5s = max stretch
        # = 120 * 5 = 600 s. Pero descartamos 30s warmup → 114 * 5 = 570.
        events = []
        for t in range(0, 600, 5):
            events.append(
                {
                    "t_seconds": float(t),
                    "src": "dkms-1",
                    "peer": "dkms-2",
                    "commodity_id": "dkms-1->dkms-2",
                    "enc": 5000,  # fill ≈ 0.076 < 0.15
                    "emit_total": t * 10,
                }
            )
        _write_generator_state(run_dir / "data" / "generator_state.csv", events)
        m = compute_metrics(run_dir, run_duration=600.0)
        # Starvation continua debe ser ≥ 500 s (descartando warmup hay 114
        # buckets * 5 s ≈ 570 s; permitimos algo de tolerancia por el
        # bucket-rounding).
        assert m.max_starvation_continuous_seconds >= 500.0


def test_verify_against_baseline_returns_5_criteria() -> None:
    """``verify_against_baseline`` produce 5 ``CriterionResult``s."""
    with tempfile.TemporaryDirectory() as tmpdir:
        run_dir = Path(tmpdir)
        # Crear un baseline mínimo y un "post-cambio" mejor.
        for label in ("baseline", "post"):
            _write_per_commodity(
                run_dir / label / "data" / "per_commodity.csv",
                [
                    {
                        "src": "d1",
                        "peer": "d2",
                        "commodity_id": "d1->d2",
                        "max_enc": 30000,
                        "saturated": "False",
                    },
                ],
            )
            _write_generator_state(
                run_dir / label / "data" / "generator_state.csv",
                [
                    {
                        "t_seconds": 500.0,
                        "src": "d1",
                        "peer": "d2",
                        "commodity_id": "d1->d2",
                        "enc": 30000,
                        "emit_total": 30000,
                    }
                ],
            )
        baseline = compute_metrics(run_dir / "baseline")
        post = compute_metrics(run_dir / "post")
        criteria = verify_against_baseline(post, baseline)
        assert len(criteria) == 5
        # Cada uno tiene los campos esperados.
        for c in criteria:
            assert isinstance(c.name, str) and c.name
            assert c.passed in (True, False)
            assert c.threshold is not None


def test_compute_metrics_missing_files_raises() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        with pytest.raises(FileNotFoundError):
            compute_metrics(Path(tmpdir))


def test_plot_compare_writes_png_when_matplotlib_present(tmp_path: Path) -> None:
    """OBJ-018 preparatorio: `plot_compare(baseline, post, out)` produce un PNG."""
    pytest.importorskip("matplotlib")
    from tests.cli.bench_multipath import plot_compare

    # Setup mínimo: 2 runs idénticos con un commodity llenando ligeramente.
    for label in ("baseline", "post"):
        run_dir = tmp_path / label
        _write_per_commodity(
            run_dir / "data" / "per_commodity.csv",
            [
                {
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "max_enc": 30000,
                    "saturated": "False",
                },
            ],
        )
        _write_generator_state(
            run_dir / "data" / "generator_state.csv",
            [
                {
                    "t_seconds": 0.0,
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "enc": 0,
                    "emit_total": 0,
                },
                {
                    "t_seconds": 300.0,
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "enc": 15000,
                    "emit_total": 15000,
                },
                {
                    "t_seconds": 600.0,
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "enc": 30000,
                    "emit_total": 30000,
                },
            ],
        )
    out_png = tmp_path / "compare.png"
    ok = plot_compare(tmp_path / "baseline", tmp_path / "post", out_png)
    assert ok is True
    assert out_png.exists()
    # Tamaño mínimo "sanity": un PNG válido ocupa al menos varios KB.
    assert out_png.stat().st_size > 5_000


def test_plot_compare_returns_false_on_missing_csvs(tmp_path: Path) -> None:
    """Si los CSVs no existen, plot_compare retorna False sin crashear."""
    from tests.cli.bench_multipath import plot_compare

    out_png = tmp_path / "compare.png"
    ok = plot_compare(tmp_path / "x", tmp_path / "y", out_png)
    assert ok is False
    assert not out_png.exists()


def test_warmup_excludes_early_events() -> None:
    """Eventos antes de warmup se ignoran para `last_state`."""
    with tempfile.TemporaryDirectory() as tmpdir:
        run_dir = Path(tmpdir)
        _write_per_commodity(
            run_dir / "data" / "per_commodity.csv",
            [
                {
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "max_enc": 50000,
                    "saturated": "False",
                },
            ],
        )
        _write_generator_state(
            run_dir / "data" / "generator_state.csv",
            [
                # Pre-warmup → ignorado
                {
                    "t_seconds": 10.0,
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "enc": 1000,
                    "emit_total": 1000,
                },
                # Post-warmup → este es el "last state"
                {
                    "t_seconds": 500.0,
                    "src": "d1",
                    "peer": "d2",
                    "commodity_id": "d1->d2",
                    "enc": 50000,
                    "emit_total": 50000,
                },
            ],
        )
        m = compute_metrics(run_dir, warmup=30.0)
        # Last state fill = 50000/65536 ≈ 0.763, NO 0.015.
        assert m.min_fill_ratio > 0.7
        assert m.total_production_keys == 50000
