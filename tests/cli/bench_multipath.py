"""Benchmark de aceptación para el cambio K-Splittable MCF.

Lee los CSVs producidos por ``analyze.py`` (``per_commodity.csv`` y
``generator_state.csv``) de un run y emite las **5 métricas de
aceptación** medibles offline definidas en
``memory/project_multipath_design.md`` (sección 12).

La 6ª métrica (coste de solver < 200 ms, memoria, binario) se mide en
Fase D con ``cargo bench`` + comparación de binario; no aquí.

Uso:

```
python3 -m tests.cli.bench_multipath <run_dir> [--baseline <run_dir>]
                                     [--buffer-size 65536]
                                     [--run-duration 600]
                                     [--warmup 30]
                                     [--starvation-threshold 0.15]
                                     [--json]
```

Si se pasa ``--baseline``, emite además la comparativa (cambio
relativo) y verifica los 5 criterios. Sin ``--baseline``, solo emite
las métricas absolutas del run (útil para fijar el baseline).
"""

from __future__ import annotations

import argparse
import csv
import json
import sys
from collections import defaultdict
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any

DEFAULT_BUFFER_SIZE: int = 65_536
DEFAULT_RUN_DURATION: float = 600.0
DEFAULT_WARMUP: float = 30.0
DEFAULT_STARVATION_THRESHOLD: float = 0.15
DEFAULT_SAT_THRESHOLD: float = 0.95

# Criterios de aceptación (project_multipath_design.md §12).
TARGET_SPREAD_MAX: float = 0.25
TARGET_MIN_FILL: float = 0.40
TARGET_MAX_PRODUCTION_DROP_PCT: float = 10.0
TARGET_SATURATION_TIME_DROP_PCT: float = 60.0
TARGET_STARVATION_MAX_SECONDS: float = 60.0


# -----------------------------------------------------------------------------
# Data model
# -----------------------------------------------------------------------------


@dataclass
class RunMetrics:
    """Métricas absolutas de un run."""

    run_dir: str
    buffer_size: int
    run_duration: float
    warmup: float
    # Métrica 1 — spread del fill ratio entre commodities en régimen
    # estacionario (max - min de los últimos fills).
    spread_fill_ratio: float
    # Métrica 2 — fill ratio mínimo por commodity en régimen estacionario.
    min_fill_ratio: float
    # Métrica 3 — producción total de claves (Σ emit_total por commodity).
    total_production_keys: int
    # Métrica 4 — segundos-buffer agregado en saturación
    # (Σ (run_duration - t_observed_seconds) para commodities saturados).
    saturation_time_seconds: float
    # Métrica 5 — máximo tiempo continuo con fill < threshold por DKMS
    # (alguno de sus buffers); aggregado como el máximo entre todos los
    # DKMS.
    max_starvation_continuous_seconds: float
    # Contextuales (no son criterios pero ayudan a contextualizar).
    n_commodities: int
    n_saturated: int
    median_fill_ratio: float


@dataclass
class CriterionResult:
    name: str
    value: float | str
    threshold: float | str | None
    passed: bool | None
    note: str = ""


# -----------------------------------------------------------------------------
# CSV loading
# -----------------------------------------------------------------------------


def _load_per_commodity(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    if not path.exists():
        raise FileNotFoundError(f"per_commodity.csv no encontrado: {path}")
    with open(path, "r", encoding="utf-8") as fh:
        reader = csv.DictReader(fh)
        for raw in reader:
            row: dict[str, Any] = {}
            for k, v in raw.items():
                if v == "" or v is None:
                    row[k] = None
                elif k == "saturated":
                    row[k] = v.lower() in ("true", "1", "yes")
                else:
                    try:
                        row[k] = int(v)
                    except ValueError:
                        try:
                            row[k] = float(v)
                        except ValueError:
                            row[k] = v
            rows.append(row)
    return rows


def _load_generator_state(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    if not path.exists():
        raise FileNotFoundError(f"generator_state.csv no encontrado: {path}")
    with open(path, "r", encoding="utf-8") as fh:
        reader = csv.DictReader(fh)
        for raw in reader:
            try:
                t_seconds = float(raw.get("t_seconds") or 0.0)
                enc = int(raw.get("enc") or 0)
                emit_total = int(raw.get("emit_total") or 0)
            except (TypeError, ValueError):
                continue
            rows.append(
                {
                    "t_seconds": t_seconds,
                    "src": raw.get("src", ""),
                    "peer": raw.get("peer", ""),
                    "commodity_id": raw.get("commodity_id", ""),
                    "enc": enc,
                    "emit_total": emit_total,
                }
            )
    return rows


# -----------------------------------------------------------------------------
# Metric computations
# -----------------------------------------------------------------------------


def _last_state_per_commodity(
    gen_state: list[dict[str, Any]], buffer_size: int, warmup: float
) -> dict[str, dict[str, float]]:
    """Para cada commodity_id, último ``enc`` y ``emit_total`` post-warmup."""
    out: dict[str, dict[str, float]] = {}
    for r in gen_state:
        if r["t_seconds"] < warmup:
            continue
        cid = r["commodity_id"]
        prev = out.get(cid)
        if prev is None or r["t_seconds"] > prev["t_seconds"]:
            out[cid] = {
                "t_seconds": r["t_seconds"],
                "enc": r["enc"],
                "emit_total": r["emit_total"],
                "fill": r["enc"] / buffer_size if buffer_size > 0 else 0.0,
                "src": r["src"],
                "peer": r["peer"],
            }
    return out


def _spread_and_min_fill(last_states: dict[str, dict[str, float]]) -> tuple[float, float, float]:
    fills = [s["fill"] for s in last_states.values()]
    if not fills:
        return 0.0, 0.0, 0.0
    fills_sorted = sorted(fills)
    median = fills_sorted[len(fills_sorted) // 2]
    return max(fills) - min(fills), min(fills), median


def _total_production(last_states: dict[str, dict[str, float]]) -> int:
    return int(sum(s["emit_total"] for s in last_states.values()))


def _saturation_time(
    per_commodity: list[dict[str, Any]], run_duration: float
) -> tuple[float, int]:
    """Σ (run_duration - t_observed_seconds) sobre commodities saturados.

    Aproxima los segundos-buffer-al-100 %: cada commodity saturado pasa
    el resto del run "lleno". Cuanto mayor el agregado, más capacidad
    QKD se desperdicia.
    """
    total = 0.0
    n_sat = 0
    for r in per_commodity:
        if not r.get("saturated"):
            continue
        t_sat = r.get("t_observed_seconds")
        if not isinstance(t_sat, (int, float)):
            continue
        total += max(0.0, run_duration - float(t_sat))
        n_sat += 1
    return total, n_sat


def _starvation_max_continuous(
    gen_state: list[dict[str, Any]],
    buffer_size: int,
    threshold: float,
    warmup: float,
    run_duration: float,
) -> float:
    """Máx. intervalo continuo donde ALGÚN buffer de cada DKMS < threshold·buffer.

    Por cada DKMS, evalúa cada bucket temporal (5 s = ritmo de
    ``generator.state``): "está alguno de sus buffers bajo threshold?".
    Encuentra el max stretch continuo de buckets afirmativos. Devuelve
    el max sobre todos los DKMS.

    Esta métrica usa la definición acordada en sec 12 del documento de
    diseño: "el DKMS está starved si alguno de sus buffers está < 0.15".
    """
    if buffer_size <= 0:
        return 0.0
    sat_fill = threshold

    # Agrupar eventos por DKMS y por bucket (round t_seconds to integer s).
    # Para cada (DKMS, t_bucket) → ¿alguno de sus buffers está < sat_fill?
    dkms_buckets: dict[str, dict[int, bool]] = defaultdict(dict)
    bucket_size_s = 5  # frecuencia del log.state
    for r in gen_state:
        if r["t_seconds"] < warmup or r["t_seconds"] > run_duration:
            continue
        src = r["src"]
        t_bucket = int(r["t_seconds"] // bucket_size_s)
        fill = r["enc"] / buffer_size
        prev = dkms_buckets[src].get(t_bucket, False)
        # OR: una sola buffer < threshold marca el bucket como starved.
        dkms_buckets[src][t_bucket] = prev or (fill < sat_fill)

    # Computar max stretch continuo de buckets True por DKMS.
    global_max_seconds = 0.0
    for src, buckets in dkms_buckets.items():
        if not buckets:
            continue
        sorted_keys = sorted(buckets.keys())
        cur_run = 0
        max_run = 0
        prev_key: int | None = None
        for k in sorted_keys:
            if not buckets[k]:
                cur_run = 0
                prev_key = k
                continue
            # Continúa el run sólo si bucket previo es adyacente y también starved.
            if prev_key is not None and k == prev_key + 1 and buckets.get(prev_key, False):
                cur_run += 1
            else:
                cur_run = 1
            max_run = max(max_run, cur_run)
            prev_key = k
        seconds = max_run * bucket_size_s
        global_max_seconds = max(global_max_seconds, seconds)
    return global_max_seconds


def compute_metrics(
    run_dir: Path,
    buffer_size: int = DEFAULT_BUFFER_SIZE,
    run_duration: float = DEFAULT_RUN_DURATION,
    warmup: float = DEFAULT_WARMUP,
    starvation_threshold: float = DEFAULT_STARVATION_THRESHOLD,
) -> RunMetrics:
    """Calcula las 5 métricas medibles offline para un run.

    Args:
        run_dir: directorio que contiene ``data/per_commodity.csv`` y
            ``data/generator_state.csv``.
        buffer_size: capacidad del buffer ENC por peer (defaults a
            65536, override si tu run usó otro).
        run_duration: duración del run en segundos.
        warmup: segundos iniciales a descartar (transitorio).
        starvation_threshold: fill ratio bajo el cual un buffer cuenta
            como starvation (default 0.15).
    """
    pc = _load_per_commodity(run_dir / "data" / "per_commodity.csv")
    gs = _load_generator_state(run_dir / "data" / "generator_state.csv")

    last_states = _last_state_per_commodity(gs, buffer_size, warmup)
    spread, min_fill, median = _spread_and_min_fill(last_states)
    total_prod = _total_production(last_states)
    sat_time, n_sat = _saturation_time(pc, run_duration)
    starvation = _starvation_max_continuous(
        gs, buffer_size, starvation_threshold, warmup, run_duration
    )

    return RunMetrics(
        run_dir=str(run_dir),
        buffer_size=buffer_size,
        run_duration=run_duration,
        warmup=warmup,
        spread_fill_ratio=spread,
        min_fill_ratio=min_fill,
        total_production_keys=total_prod,
        saturation_time_seconds=sat_time,
        max_starvation_continuous_seconds=starvation,
        n_commodities=len(pc),
        n_saturated=n_sat,
        median_fill_ratio=median,
    )


# -----------------------------------------------------------------------------
# Acceptance verification (vs baseline)
# -----------------------------------------------------------------------------


def verify_against_baseline(
    post: RunMetrics, baseline: RunMetrics
) -> list[CriterionResult]:
    """Compara métricas post-cambio vs baseline contra los 5 umbrales.

    Devuelve una lista de ``CriterionResult`` con `passed=True/False/None`.
    """
    results: list[CriterionResult] = []

    # C1: spread ≤ 0.25 (absoluto, no relativo a baseline).
    results.append(
        CriterionResult(
            name="C1: Spread fill ratio (régimen estacionario)",
            value=round(post.spread_fill_ratio, 4),
            threshold=f"≤ {TARGET_SPREAD_MAX}",
            passed=post.spread_fill_ratio <= TARGET_SPREAD_MAX,
            note=f"baseline = {round(baseline.spread_fill_ratio, 4)}",
        )
    )

    # C2: min fill ratio ≥ 0.40 (absoluto).
    results.append(
        CriterionResult(
            name="C2: Min fill ratio a los 600s",
            value=round(post.min_fill_ratio, 4),
            threshold=f"≥ {TARGET_MIN_FILL}",
            passed=post.min_fill_ratio >= TARGET_MIN_FILL,
            note=f"baseline = {round(baseline.min_fill_ratio, 4)}",
        )
    )

    # C3: producción total no cae más del 10 % vs baseline.
    if baseline.total_production_keys > 0:
        drop_pct = (
            (baseline.total_production_keys - post.total_production_keys)
            / baseline.total_production_keys
        ) * 100.0
    else:
        drop_pct = 0.0
    results.append(
        CriterionResult(
            name="C3: Caída de producción total (%)",
            value=round(drop_pct, 2),
            threshold=f"≤ {TARGET_MAX_PRODUCTION_DROP_PCT}",
            passed=drop_pct <= TARGET_MAX_PRODUCTION_DROP_PCT,
            note=(
                f"baseline = {baseline.total_production_keys}, "
                f"post = {post.total_production_keys}"
            ),
        )
    )

    # C4: tiempo agregado en saturación cae ≥ 60 % vs baseline.
    if baseline.saturation_time_seconds > 0:
        drop_pct_sat = (
            (baseline.saturation_time_seconds - post.saturation_time_seconds)
            / baseline.saturation_time_seconds
        ) * 100.0
    else:
        drop_pct_sat = 0.0
    results.append(
        CriterionResult(
            name="C4: Caída de tiempo-saturación agregado (%)",
            value=round(drop_pct_sat, 2),
            threshold=f"≥ {TARGET_SATURATION_TIME_DROP_PCT}",
            passed=drop_pct_sat >= TARGET_SATURATION_TIME_DROP_PCT,
            note=(
                f"baseline = {round(baseline.saturation_time_seconds, 1)} s, "
                f"post = {round(post.saturation_time_seconds, 1)} s"
            ),
        )
    )

    # C5: starvation continua por DKMS ≤ 60 s (absoluto).
    results.append(
        CriterionResult(
            name="C5: Starvation continua máxima por DKMS (s)",
            value=round(post.max_starvation_continuous_seconds, 1),
            threshold=f"≤ {TARGET_STARVATION_MAX_SECONDS}",
            passed=post.max_starvation_continuous_seconds <= TARGET_STARVATION_MAX_SECONDS,
            note=f"baseline = {round(baseline.max_starvation_continuous_seconds, 1)} s",
        )
    )

    return results


# -----------------------------------------------------------------------------
# Comparative plot (OBJ-018)
# -----------------------------------------------------------------------------


def _import_matplotlib():
    """Lazy import — falla silenciosa si matplotlib no instalado."""
    try:
        import matplotlib  # type: ignore

        matplotlib.use("Agg")
        import matplotlib.pyplot as plt  # type: ignore

        return plt
    except Exception:  # noqa: BLE001
        return None


def plot_compare(
    baseline_run: Path, post_run: Path, output_path: Path, buffer_size: int = DEFAULT_BUFFER_SIZE
) -> bool:
    """Gráfica comparativa 2-paneles: ENC buffer fill baseline vs post.

    Cada panel muestra una línea por commodity sobre el tiempo. Eje Y
    es `enc` (claves en buffer ENC). El umbral de saturación
    (0.95·buffer_size) se marca con línea horizontal punteada.

    Devuelve True si la gráfica se escribió, False si matplotlib no
    está disponible o los CSVs están vacíos.

    Pensado para OBJ-018: alimentar baseline + post de la misma
    topología y producir una gráfica visualizable.
    """
    plt = _import_matplotlib()
    if plt is None:
        return False

    try:
        baseline_gs = _load_generator_state(
            baseline_run / "data" / "generator_state.csv"
        )
        post_gs = _load_generator_state(post_run / "data" / "generator_state.csv")
    except FileNotFoundError:
        return False
    if not baseline_gs or not post_gs:
        return False

    sat_y = 0.95 * buffer_size

    fig, axes = plt.subplots(1, 2, figsize=(16, 6), sharey=True)

    for ax, label, gs in [
        (axes[0], f"Baseline ({baseline_run.name})", baseline_gs),
        (axes[1], f"Post-cambio ({post_run.name})", post_gs),
    ]:
        # Agrupar por commodity_id y plotear cada serie.
        by_c: dict[str, list[tuple[float, int]]] = {}
        for r in gs:
            by_c.setdefault(r["commodity_id"], []).append(
                (r["t_seconds"], r["enc"])
            )
        for series in by_c.values():
            series.sort(key=lambda t: t[0])
            xs = [t for t, _ in series]
            ys = [e for _, e in series]
            ax.plot(xs, ys, linewidth=0.8, alpha=0.5)
        ax.axhline(
            sat_y,
            linestyle="--",
            linewidth=1.0,
            color="black",
            label=f"sat threshold (0.95·{buffer_size}={int(sat_y)})",
        )
        ax.set_xlabel("seconds since first event")
        ax.set_ylabel("enc (keys in ENC buffer)")
        ax.set_title(f"{label} — {len(by_c)} commodities")
        ax.set_ylim(bottom=0)
        ax.legend(loc="lower right", fontsize=8)

    fig.suptitle(
        "ENC buffer fill — Baseline vs Post-cambio (K-Splittable MCF)",
        fontsize=12,
    )
    fig.tight_layout()
    output_path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(output_path, dpi=100)
    plt.close(fig)
    return True


# -----------------------------------------------------------------------------
# Reporting
# -----------------------------------------------------------------------------


def _format_report(
    metrics: RunMetrics, criteria: list[CriterionResult] | None = None
) -> str:
    lines: list[str] = []
    lines.append(f"\n=== Bench Multipath — run {metrics.run_dir} ===")
    lines.append(f"buffer_size       : {metrics.buffer_size}")
    lines.append(
        f"run_duration      : {metrics.run_duration} s "
        f"(warmup descartado: {metrics.warmup} s)"
    )
    lines.append(
        f"commodities       : {metrics.n_commodities} totales, "
        f"{metrics.n_saturated} saturados "
        f"({100 * metrics.n_saturated / max(metrics.n_commodities, 1):.1f} %)"
    )
    lines.append(f"\nMétricas absolutas:")
    lines.append(f"  M1 spread_fill_ratio             : {metrics.spread_fill_ratio:.4f}")
    lines.append(f"  M2 min_fill_ratio                : {metrics.min_fill_ratio:.4f}")
    lines.append(f"  M3 total_production_keys         : {metrics.total_production_keys}")
    lines.append(f"  M4 saturation_time_seconds       : {metrics.saturation_time_seconds:.1f}")
    lines.append(
        f"  M5 max_starvation_continuous_sec : {metrics.max_starvation_continuous_seconds:.1f}"
    )
    lines.append(f"  median_fill_ratio                : {metrics.median_fill_ratio:.4f}")

    if criteria:
        lines.append("\nCriterios de aceptación:")
        passed_count = sum(1 for c in criteria if c.passed)
        for c in criteria:
            mark = "✓" if c.passed else ("✗" if c.passed is False else "?")
            lines.append(
                f"  {mark} {c.name}: {c.value} (umbral {c.threshold}). {c.note}"
            )
        lines.append(
            f"\nResumen: {passed_count}/{len(criteria)} criterios cumplidos."
        )
    return "\n".join(lines)


# -----------------------------------------------------------------------------
# CLI
# -----------------------------------------------------------------------------


def _parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    p = argparse.ArgumentParser(
        prog="bench_multipath",
        description=(
            "Calcula las 5 métricas de aceptación del cambio K-Splittable "
            "MCF a partir de los CSVs de un run (data/per_commodity.csv y "
            "data/generator_state.csv)."
        ),
    )
    p.add_argument("run_dir", help="directorio del run a analizar")
    p.add_argument(
        "--baseline",
        default=None,
        help="directorio del run baseline para comparar (opcional)",
    )
    p.add_argument(
        "--buffer-size",
        type=int,
        default=DEFAULT_BUFFER_SIZE,
        help=f"capacidad del buffer ENC (default {DEFAULT_BUFFER_SIZE})",
    )
    p.add_argument(
        "--run-duration",
        type=float,
        default=DEFAULT_RUN_DURATION,
        help=f"duración del run en segundos (default {DEFAULT_RUN_DURATION})",
    )
    p.add_argument(
        "--warmup",
        type=float,
        default=DEFAULT_WARMUP,
        help=f"segundos a descartar al inicio (default {DEFAULT_WARMUP})",
    )
    p.add_argument(
        "--starvation-threshold",
        type=float,
        default=DEFAULT_STARVATION_THRESHOLD,
        help=(
            f"fill ratio bajo el cual un buffer cuenta como starvation "
            f"(default {DEFAULT_STARVATION_THRESHOLD})"
        ),
    )
    p.add_argument(
        "--json",
        action="store_true",
        help="emite las métricas como JSON en stdout",
    )
    p.add_argument(
        "--plot-compare",
        default=None,
        help=(
            "ruta de salida (PNG) para la gráfica comparativa baseline vs "
            "post. Requiere --baseline. Sin matplotlib se omite silenciosamente."
        ),
    )
    return p.parse_args(argv)


def run(argv: list[str] | None = None) -> int:
    args = _parse_args(argv)
    run_dir = Path(args.run_dir)
    post = compute_metrics(
        run_dir,
        buffer_size=args.buffer_size,
        run_duration=args.run_duration,
        warmup=args.warmup,
        starvation_threshold=args.starvation_threshold,
    )

    criteria: list[CriterionResult] | None = None
    if args.baseline:
        baseline = compute_metrics(
            Path(args.baseline),
            buffer_size=args.buffer_size,
            run_duration=args.run_duration,
            warmup=args.warmup,
            starvation_threshold=args.starvation_threshold,
        )
        criteria = verify_against_baseline(post, baseline)

    if args.json:
        payload: dict[str, Any] = {"post": asdict(post)}
        if criteria is not None:
            payload["criteria"] = [asdict(c) for c in criteria]
            payload["passed_count"] = sum(1 for c in criteria if c.passed)
            payload["total_count"] = len(criteria)
        json.dump(payload, sys.stdout, indent=2, default=str)
        sys.stdout.write("\n")
    else:
        sys.stdout.write(_format_report(post, criteria))
        sys.stdout.write("\n")

    # Gráfica comparativa opcional.
    if args.plot_compare and args.baseline:
        plot_out = Path(args.plot_compare)
        if plot_compare(
            Path(args.baseline),
            run_dir,
            plot_out,
            buffer_size=args.buffer_size,
        ):
            sys.stderr.write(f"plot-compare escrito en {plot_out}\n")
        else:
            sys.stderr.write(
                "warning: plot-compare omitido (matplotlib ausente o CSVs vacíos)\n"
            )

    if criteria is not None:
        # Exit code = 0 sólo si todos los criterios cumplen.
        return 0 if all(c.passed for c in criteria) else 1
    return 0


def main() -> None:  # pragma: no cover
    sys.exit(run())


if __name__ == "__main__":  # pragma: no cover
    main()
