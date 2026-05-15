#!/usr/bin/env python3
"""Genera gráficas del ramp test de SAEs.

Lee /tmp/dkms-star-demo/ramp/sae_*.log y /tmp/dkms-star-demo/ramp/start_unix.txt
y emite varias gráficas en /tmp/dkms-star-demo/ramp/plots/:

    1. ok_rate_per_sae.png    — rate de 200s por SAE individual a lo largo del tiempo
    2. rate_429_per_sae.png   — rate de 429s por SAE
    3. aggregate_rate.png     — suma de todos los SAEs (200 vs 429), con la línea
                                de número activo de SAEs
    4. mean_rate_vs_n.png     — rate medio por SAE frente al N_SAEs activos
    5. fairness_box.png       — boxplot de la rate por SAE en cada step

Bucketiza por segundos (resampling 1s).
"""
from __future__ import annotations

import collections
import csv
import glob
import os
import statistics
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")  # no GUI
import matplotlib.pyplot as plt


LOGS_DIR = Path("/tmp/dkms-star-demo/ramp")
OUT_DIR = LOGS_DIR / "plots"


def read_start_t0() -> float:
    p = LOGS_DIR / "start_unix.txt"
    if not p.exists():
        print(f"falta {p}", file=sys.stderr)
        sys.exit(2)
    return float(p.read_text().strip())


def read_assignment() -> dict[str, dict[str, str]]:
    """sae_id -> {home_dkms, slave_sae, ...}"""
    out = {}
    p = LOGS_DIR / "assignment.csv"
    if not p.exists():
        return out
    with p.open() as f:
        r = csv.DictReader(f)
        for row in r:
            out[row["sae_id"]] = row
    return out


def parse_sae_log(path: Path, t0: float) -> dict[str, list]:
    """Devuelve dict con timestamps relativos a t0 separados por status."""
    by_status: dict[str, list[float]] = collections.defaultdict(list)
    with path.open() as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            parts = line.split()
            if len(parts) != 2:
                continue
            try:
                ts = float(parts[0]) - t0
            except ValueError:
                continue
            by_status[parts[1]].append(ts)
    return by_status


def bucket_per_second(timestamps: list[float], total_seconds: int) -> list[int]:
    """count[t]  con t in [0, total_seconds)."""
    counts = [0] * max(1, total_seconds)
    for ts in timestamps:
        s = int(ts)
        if 0 <= s < total_seconds:
            counts[s] += 1
    return counts


def main() -> int:
    t0 = read_start_t0()
    assignment = read_assignment()
    sae_files = sorted(LOGS_DIR.glob("sae_*.log"))
    if not sae_files:
        print(f"no hay logs sae_*.log en {LOGS_DIR}", file=sys.stderr)
        return 2

    # Determina la duración total: max timestamp visto + 1s
    max_t = 0.0
    per_sae: dict[str, dict[str, list[float]]] = {}
    for f in sae_files:
        sae = f.stem
        data = parse_sae_log(f, t0)
        per_sae[sae] = data
        for ts_list in data.values():
            if ts_list:
                max_t = max(max_t, max(ts_list))
    total_seconds = int(max_t) + 2

    # Rate por SAE por segundo (200 y 429 separados)
    ok_per_sae: dict[str, list[int]] = {}
    rl_per_sae: dict[str, list[int]] = {}
    for sae, data in per_sae.items():
        ok_per_sae[sae] = bucket_per_second(data.get("200", []), total_seconds)
        rl_per_sae[sae] = bucket_per_second(data.get("429", []), total_seconds)

    # SAEs "activos" por segundo: aquellos que tienen alguna petición
    # (200, 429 o ERR) en esa ventana o anterior.
    active_per_second = [0] * total_seconds
    for sae, data in per_sae.items():
        first_ts = None
        for ts_list in data.values():
            if ts_list:
                first_ts = min(ts_list) if first_ts is None else min(first_ts, min(ts_list))
        if first_ts is None:
            continue
        for s in range(max(0, int(first_ts)), total_seconds):
            active_per_second[s] += 1

    OUT_DIR.mkdir(parents=True, exist_ok=True)

    # ────────── 1. ok rate por SAE ─────────────────────────────────────
    fig, ax = plt.subplots(figsize=(12, 6))
    for sae in sorted(ok_per_sae):
        ax.plot(range(total_seconds), ok_per_sae[sae], alpha=0.4, linewidth=0.7)
    ax.set_title("Per-SAE enc_keys 200/s rate (each line = 1 SAE)")
    ax.set_xlabel("tiempo (s desde t=0)")
    ax.set_ylabel("keys 200/s por SAE")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(OUT_DIR / "ok_rate_per_sae.png", dpi=120)
    plt.close(fig)

    # ────────── 2. 429 rate por SAE ────────────────────────────────────
    fig, ax = plt.subplots(figsize=(12, 6))
    for sae in sorted(rl_per_sae):
        ax.plot(range(total_seconds), rl_per_sae[sae], alpha=0.4, linewidth=0.7, color="red")
    ax.set_title("Per-SAE rate-limited (HTTP 429) /s")
    ax.set_xlabel("tiempo (s)")
    ax.set_ylabel("429/s por SAE")
    ax.grid(True, alpha=0.3)
    fig.tight_layout()
    fig.savefig(OUT_DIR / "rate_429_per_sae.png", dpi=120)
    plt.close(fig)

    # ────────── 3. aggregate + active count ────────────────────────────
    fig, ax1 = plt.subplots(figsize=(12, 6))
    agg_ok = [sum(ok_per_sae[s][t] for s in ok_per_sae) for t in range(total_seconds)]
    agg_rl = [sum(rl_per_sae[s][t] for s in rl_per_sae) for t in range(total_seconds)]
    ax1.plot(range(total_seconds), agg_ok, label="200/s aggregate", color="green", linewidth=1.5)
    ax1.plot(range(total_seconds), agg_rl, label="429/s aggregate", color="red", linewidth=1.5)
    ax1.set_xlabel("tiempo (s)")
    ax1.set_ylabel("requests/s (sumado)")
    ax1.grid(True, alpha=0.3)
    ax1.legend(loc="upper left")
    ax2 = ax1.twinx()
    ax2.plot(range(total_seconds), active_per_second, label="SAEs activos", color="blue", linewidth=2.0, linestyle="--")
    ax2.set_ylabel("# SAEs activos", color="blue")
    ax2.tick_params(axis="y", labelcolor="blue")
    ax2.legend(loc="upper right")
    plt.title("Aggregate enc_keys rate + #SAEs activos")
    fig.tight_layout()
    fig.savefig(OUT_DIR / "aggregate_rate.png", dpi=120)
    plt.close(fig)

    # ────────── 4. mean rate vs N (curva 1/N esperada) ────────────────
    # Para cada t, calcula media de la rate por SAE activo.
    mean_rates: list[tuple[int, float]] = []
    for t in range(total_seconds):
        n = active_per_second[t]
        if n <= 0:
            continue
        # Suma solo los SAEs que tienen alguna actividad ya:
        per_sae_rates_this_t = []
        for sae in ok_per_sae:
            # Solo cuenta el SAE si su primera petición ya pasó
            data = per_sae[sae]
            all_ts = sum(data.values(), [])
            if not all_ts or min(all_ts) > t:
                continue
            per_sae_rates_this_t.append(ok_per_sae[sae][t])
        if per_sae_rates_this_t:
            mean_rates.append((n, statistics.mean(per_sae_rates_this_t)))
    fig, ax = plt.subplots(figsize=(10, 6))
    if mean_rates:
        ns, rs = zip(*mean_rates)
        ax.scatter(ns, rs, s=8, alpha=0.5, label="muestra (1s)")
        # Ajuste hiperbólico esperado: rate ≈ C / N
        if rs:
            c_est = statistics.median([n * r for n, r in zip(ns, rs) if r > 0])
            n_range = sorted(set(ns))
            ax.plot(n_range, [c_est / n if n else 0 for n in n_range],
                    "k--", label=f"y = {c_est:.0f}/N")
        ax.set_xlabel("# SAEs activos compartiendo enlace")
        ax.set_ylabel("rate media por SAE (keys/s)")
        ax.set_title("Mean per-SAE enc_keys/s vs SAEs activos")
        ax.grid(True, alpha=0.3)
        ax.legend()
    fig.tight_layout()
    fig.savefig(OUT_DIR / "mean_rate_vs_n.png", dpi=120)
    plt.close(fig)

    # ────────── 5. resumen impreso ─────────────────────────────────────
    print("──── resumen ramp test ────")
    print(f"SAEs analizados: {len(per_sae)}")
    print(f"Duración total : {total_seconds} s")
    total_ok = sum(sum(v) for v in ok_per_sae.values())
    total_rl = sum(sum(v) for v in rl_per_sae.values())
    print(f"Total 200      : {total_ok}")
    print(f"Total 429      : {total_rl}")
    if total_ok + total_rl > 0:
        print(f"% rate-limited : {100*total_rl/(total_ok+total_rl):.1f} %")
    if agg_ok:
        avg_agg = sum(agg_ok) / total_seconds
        print(f"Aggregate medio: {avg_agg:.1f} keys/s")
    print(f"\nplots en {OUT_DIR}/")
    return 0


if __name__ == "__main__":
    sys.exit(main())
