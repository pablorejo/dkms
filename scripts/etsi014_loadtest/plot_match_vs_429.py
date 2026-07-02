"""Plot: match-rate vs 429-rate + nº de SAEs activos a lo largo del tiempo.

Uso: python3 plot_match_vs_429.py <run_dir>
    run_dir debe contener requests.csv (idealmente con columna worker_id).
"""
from __future__ import annotations
import argparse, csv, pathlib, json
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np


def main(run_dir: pathlib.Path):
    rows = list(csv.DictReader((run_dir / "requests.csv").open()))
    print(f"loaded {len(rows)} rows")
    if not rows:
        return

    has_worker = "worker_id" in rows[0]
    summary = {}
    sj = run_dir / "summary.json"
    if sj.exists():
        summary = json.loads(sj.read_text())

    t = np.array([float(r["t_emit"]) for r in rows])
    t0 = t.min()
    t_rel = t - t0
    duration = float(t_rel.max()) + 1

    # 1s bins.
    bins = np.arange(0, duration + 1, 1.0)
    centers = (bins[:-1] + bins[1:]) / 2

    # Match RPS (bytes idénticos).
    match_t = t_rel[np.array([r["match"] == "1" for r in rows])]
    hist_match, _ = np.histogram(match_t, bins=bins)

    # HTTP 429 RPS.
    is_429 = np.array(["HTTP 429" in r["err"] for r in rows])
    h429_t = t_rel[is_429]
    hist_429, _ = np.histogram(h429_t, bins=bins)

    # SAEs activos a lo largo del tiempo = NIVEL REAL del ramp, no una muestra
    # por bin. Cada par (worker_id, pair_id) = 2 SAEs (master + slave) y está
    # "activo" desde su PRIMER request en adelante (el ramp solo añade pares
    # hasta el teardown). Por tanto
    #     active_saes(t) = 2 · #{pares cuyo primer t_emit ≤ t}
    # es monótona y recupera el ramp verdadero.
    #
    # NO usar el conteo de pares únicos por bin de 1 s: con llegadas Poisson
    # (λ≈1/s) solo ~63 % (1−e⁻¹) de los pares dispara en un bin dado, así que
    # ese proxy SUBESTIMA el nivel real ~37 % y se satura en ~10 000–10 500
    # SAEs aunque el ramp llegue a 16 000 (artefacto, no techo del experimento).
    if has_worker:
        # Identificador único de par = (worker_id, pair_id).
        pair_keys = [(int(r["worker_id"]), int(r["pair_id"])) for r in rows]
    else:
        pair_keys = [(0, int(r["pair_id"])) for r in rows]

    first_emit: dict[tuple, float] = {}
    for pk, te in zip(pair_keys, t):
        if pk not in first_emit or te < first_emit[pk]:
            first_emit[pk] = te
    # Tiempos de activación (relativos al primer request global) ordenados;
    # nº de pares activos en el centro de cada bin vía búsqueda binaria.
    activations = np.sort(np.array([v - t0 for v in first_emit.values()]))
    active_sae = 2 * np.searchsorted(activations, centers, side="right")

    # Plot.
    fig, ax1 = plt.subplots(figsize=(13, 6))
    color_match = "seagreen"
    color_429 = "firebrick"
    color_sae = "steelblue"

    ax1.plot(centers, hist_match, label="match (round-trip OK)", color=color_match, lw=1.7)
    ax1.plot(centers, hist_429, label="HTTP 429 (rate-limit)", color=color_429, lw=1.7)
    ax1.set_xlabel("seconds since first request")
    ax1.set_ylabel("requests / s", color="black")
    ax1.grid(True, alpha=0.3)
    ax1.legend(loc="upper left")

    # 2º eje: SAEs activos.
    ax2 = ax1.twinx()
    ax2.plot(centers, active_sae, label="active SAEs (ramp level)", color=color_sae, lw=1.4, ls=":")
    ax2.set_ylabel("active SAEs", color=color_sae)
    ax2.tick_params(axis="y", labelcolor=color_sae)
    ax2.legend(loc="upper right")

    # Misma escala en ambos ejes Y: lo que ocupa la tasa (requests/s, izq) es
    # directamente comparable en altura con el nº de SAEs (der). Idéntico rango
    # en los dos lados → un valor cae al mismo píxel en ambos ejes. Al compartir
    # límites, el auto-locator da las MISMAS marcas en ambos ejes (rejilla
    # alineada). El tope se redondea a un múltiplo de 2000 con algo de margen.
    mx = float(max(active_sae.max(), hist_match.max(), hist_429.max()))
    ymax = (np.floor(mx / 2000.0) + 1) * 2000.0
    ax1.set_ylim(0, ymax)
    ax2.set_ylim(0, ymax)

    title_lines = []
    title_lines.append("ETSI 014 round-trip: match vs HTTP 429 vs active SAEs")
    if summary:
        tot = summary.get("total_requests", "?")
        mp = summary.get("match_pct", "?")
        h4 = summary.get("http_errors_from_server", {})
        n429 = h4.get("enc HTTP 429", 0) + h4.get("dec HTTP 429", 0)
        title_lines.append(f"(total={tot}, match={mp}%, 429={n429})")
    ax1.set_title(" — ".join(title_lines))

    fig.tight_layout()
    out = run_dir / "plots" / "match_vs_429.png"
    out.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(out, dpi=130)
    print(f"wrote {out}")


if __name__ == "__main__":
    p = argparse.ArgumentParser()
    p.add_argument("run_dir")
    main(pathlib.Path(p.parse_args().run_dir))
