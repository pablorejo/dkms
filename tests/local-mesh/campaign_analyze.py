#!/usr/bin/env python3
"""Análisis de la campaña 2026-09 (corre en el portátil, Python ≥ 3.10 +
matplotlib): 6 familias × N=10..100 × 3 cargas → métricas por celda, tablas
markdown, figuras (PNG + SVG) y un `metrics.json` que consume la web.

    campaign_analyze.py [--cells DIR] [--out DIR]

Entrada: un directorio por celda (`<familia>-n<N>/`) con lo que deja
`campaign_cell.sh`: topology.json, meta.json, samples.txt(.gz), L1/L2
summary.json + persec.csv + pairs.csv, keys_*.log, bootstrap_times.txt.

Lo que se calcula por celda (y de dónde sale):

  L0 (reposo / llenado)   t_full (meta), pendiente de llenado = ΔΣenc/Δt sobre
                          la parte lineal del muestreo (líneas D), fracción de
                          pares a tope al cierre, tasa medida por el estimador
                          del QKC (líneas Q: rate_sum/rate_n, rate_q).
  L1 (media, pautada)     ofrecido (meta) vs servido (summary ok_keys/span),
                          429/503, latencia p50/p99, ENC vacío (D enc_zero).
  L2 (saturación)         servido bruto = ok_keys/span; SOSTENIDO CORREGIDO =
                          (claves servidas − stock drenado)/ventana, con el
                          stock = Σenc de todos los pares (D) al principio y
                          al final de la ventana estacionaria (los últimos
                          2/3 de L2). Es la corrección que la campaña N=30
                          no pudo hacer: sin ella la cifra "sostenida" incluye
                          el almacén de 4096×pares claves embalsadas.
  REC                     t_recover (meta) y pendiente de rellenado.
  Salud                   meta.health (+ keys_*.log: pares idénticos/fallidos).
  Teoría                  topology.json: techo_fibra, techo_cuello, saltos.
  Recursos                líneas P: RSS y %CPU por tipo de proceso (pico y
                          media en L2), loadavg.
"""
import argparse
import csv
import glob
import gzip
import json
import math
import os
import re
import sys
from collections import defaultdict

FAMILIES = ["estrella", "anillo", "puente", "malla", "rgg", "aleatoria"]
FAM_LABEL = {"estrella": "Estrella", "anillo": "Anillo (C_N)", "puente": "Puente",
             "malla": "Malla (rejilla)", "rgg": "RGG (QKD a distancia)", "aleatoria": "Aleatoria"}
NS = list(range(10, 101, 10))


def open_maybe_gz(path):
    if os.path.exists(path + ".gz"):
        return gzip.open(path + ".gz", "rt", encoding="utf-8", errors="replace")
    return open(path, encoding="utf-8", errors="replace")


def kv_line(line):
    out = {}
    for tok in line.split():
        if "=" in tok:
            k, v = tok.split("=", 1)
            out[k] = v.strip('"')
    return out


def parse_samples(path):
    """→ lista de muestras: {t, phase, up, D:[...], Q:[...], O:[...], P:{comm:{n,rss,cpu}}, loadavg}."""
    samples = []
    cur = None
    with open_maybe_gz(path) as fh:
        for line in fh:
            if line.startswith("=== t="):
                m = re.match(r"=== t=(\d+) phase=(\S+) up=(\d+)", line)
                if not m:
                    continue
                cur = {"t": int(m.group(1)), "phase": m.group(2), "up": int(m.group(3)),
                       "D": [], "Q": [], "O": [], "K": [], "P": {}, "loadavg": None}
                samples.append(cur)
            elif cur is None:
                continue
            elif line.startswith("D "):
                cur["D"].append(kv_line(line[2:]))
            elif line.startswith("Q "):
                cur["Q"].append(kv_line(line[2:]))
            elif line.startswith("O "):
                cur["O"].append(kv_line(line[2:]))
            elif line.startswith("K idx="):
                cur["K"].append(kv_line(line[2:]))
            elif line.startswith("P comm="):
                d = kv_line(line[2:])
                cur["P"][d["comm"]] = {"n": int(d["n"]), "rss_mb": float(d["rss_mb"]), "cpu": float(d["cpu"])}
            elif line.startswith("P loadavg="):
                cur["loadavg"] = float(line.split("=", 1)[1].split()[0])
    return samples


def fnum(d, k, default=0.0):
    try:
        return float(d.get(k, default))
    except ValueError:
        return default


def sum_enc(sample):
    return sum(fnum(d, "enc") for d in sample["D"])


def phase_samples(samples, phase):
    return [s for s in samples if s["phase"] == phase]


def linear_slope(points):
    """Pendiente (unidades/s) por mínimos cuadrados sobre (t, y)."""
    if len(points) < 2:
        return 0.0
    n = len(points)
    mx = sum(p[0] for p in points) / n
    my = sum(p[1] for p in points) / n
    sxx = sum((p[0] - mx) ** 2 for p in points)
    if sxx == 0:
        return 0.0
    return sum((p[0] - mx) * (p[1] - my) for p in points) / sxx


def read_persec(path):
    rows = []
    if not os.path.exists(path):
        return rows
    with open(path) as fh:
        for r in csv.DictReader(fh):
            rows.append({k: float(v) for k, v in r.items()})
    return rows


def keys_log_result(path):
    """`mesh.sh keys` → (pares, idénticos, fallidos) de la última línea."""
    if not os.path.exists(path):
        return None
    last = ""
    with open(path, errors="replace") as fh:
        for line in fh:
            if "pares ordenados" in line:
                last = line
    m = re.search(r"pares ordenados:\s*(\d+)\s+idénticos:\s*(\d+)\s+fallidos:\s*(\d+)", last)
    if not m:
        return None
    return tuple(int(x) for x in m.groups())


def analyze_cell(cdir):
    fam, n = os.path.basename(cdir).rsplit("-n", 1)
    n = int(n)
    topo = json.load(open(os.path.join(cdir, "topology.json")))
    meta = json.load(open(os.path.join(cdir, "meta.json"))) if os.path.exists(os.path.join(cdir, "meta.json")) else {}
    samples = parse_samples(os.path.join(cdir, "samples.txt"))
    pairs = topo["pairs"]
    cap_stock = pairs * 4096
    out = {"family": fam, "n": n, "edges": topo["edges"], "pairs": pairs,
           "mean_hops": topo["mean_hops"], "diameter": topo["diameter"],
           "degree_mean": topo["degree_mean"],
           "techo_fibra": topo["techo_fibra_keys_per_s"], "techo_cuello": topo["techo_cuello_keys_per_s"],
           "cap_min": topo["cap_min"], "cap_max": topo["cap_max"],
           "t_up_s": meta.get("t_up_s"), "t_full_s": meta.get("t_full_s"), "t_recover_s": meta.get("t_recover_s"),
           "health": meta.get("health", {}), "host": meta.get("host"), "git_sha": meta.get("git_sha"),
           "done": os.path.exists(os.path.join(cdir, "DONE")), "failed": os.path.exists(os.path.join(cdir, "FAILED"))}
    # ── L0: llenado ──
    l0 = phase_samples(samples, "L0")
    if l0:
        pts = [(s["t"], sum_enc(s)) for s in l0 if s["D"]]
        # parte lineal: hasta que Σenc alcanza el 90 % del tope o termina
        lin = [p for p in pts if p[1] < 0.9 * cap_stock] or pts
        out["l0_fill_slope_keys_per_s"] = linear_slope(lin[: max(2, len(lin))])
        out["l0_final_stock_frac"] = pts[-1][1] / cap_stock if pts and cap_stock else None
        last = l0[-1]
        out["l0_pairs_full_frac"] = (sum(fnum(d, "enc_full") for d in last["D"]) / pairs) if last["D"] else None
        rates = [(fnum(q, "rate_sum") / fnum(q, "rate_n")) for q in last["Q"] if fnum(q, "rate_n") > 0]
        out["l0_estimator_rate_mean"] = (sum(rates) / len(rates)) if rates else None
        qs = defaultdict(int)
        for q in last["Q"]:
            for part in q.get("rate_q", "").split(","):
                if ":" in part:
                    k, v = part.split(":")
                    qs[k] += int(v)
        out["l0_estimator_quality"] = dict(qs)
        out["l0_sdn_rate_zero_frac"] = (sum(fnum(d, "sdn_zero") for d in last["D"]) / pairs) if last["D"] else None
        kme = [fnum(k, "stored") / max(1.0, fnum(k, "max")) for k in last["K"]]
        out["l0_kme_fill_frac_mean"] = (sum(kme) / len(kme)) if kme else None
    # ── L1 / L2 ──
    for tag in ("L1", "L2"):
        sp = os.path.join(cdir, tag + ".summary.json")
        if not os.path.exists(sp):
            continue
        sm = json.load(open(sp))
        span = sm.get("span_s") or 0.0
        served = (sm["ok_keys"] / span) if span else 0.0
        out[tag] = {"ok_keys": sm["ok_keys"], "span_s": span, "served_keys_per_s": served,
                    "n429": sm["n429"], "n503": sm["n503"], "n_other": sm["n_other"],
                    "reject_frac": (sm["n429"] + sm["n503"] + sm["n_other"]) / max(1, sm["ok_req"] + sm["n429"] + sm["n503"] + sm["n_other"]),
                    "lat_p50_ms": sm["lat_p50_ms"], "lat_p90_ms": sm["lat_p90_ms"], "lat_p99_ms": sm["lat_p99_ms"],
                    "pairs_zero": sm["pairs_zero"], "pair_keys_min": sm["pair_keys_min"],
                    "pair_keys_median": sm["pair_keys_median"], "pair_keys_max": sm["pair_keys_max"]}
        # Equidad entre pares (claves servidas por par ordenado en la fase):
        # índice de Jain y cuánto se lleva el peor par respecto al reparto
        # uniforme. Es la otra cara del agregado α-fair.
        pp = os.path.join(cdir, tag + ".pairs.csv")
        if os.path.exists(pp):
            vals = []
            with open(pp) as fh:
                for r in csv.DictReader(fh):
                    vals.append(float(r["ok_keys"]))
            if vals:
                mean = sum(vals) / len(vals)
                out[tag]["jain_index"] = (sum(vals) ** 2) / (len(vals) * sum(v * v for v in vals)) if sum(v * v for v in vals) > 0 else None
                out[tag]["min_pair_share"] = (min(vals) / mean) if mean > 0 else None
                out[tag]["p10_pair_share"] = (sorted(vals)[len(vals) // 10] / mean) if mean > 0 else None
        ph = phase_samples(samples, tag)
        if ph:
            enc_zero = [sum(fnum(d, "enc_zero") for d in s["D"]) / pairs for s in ph if s["D"]]
            out[tag]["enc_zero_frac_mean"] = (sum(enc_zero) / len(enc_zero)) if enc_zero else None
            cpu = defaultdict(list)
            rss = defaultdict(list)
            for s in ph:
                for comm, v in s["P"].items():
                    cpu[comm].append(v["cpu"])
                    rss[comm].append(v["rss_mb"])
            out[tag]["cpu_mean_pct"] = {c: sum(v) / len(v) for c, v in cpu.items()}
            out[tag]["rss_peak_mb"] = {c: max(v) for c, v in rss.items()}
            out[tag]["rss_total_peak_mb"] = max((sum(s["P"][c]["rss_mb"] for c in s["P"]) for s in ph if s["P"]), default=None)
            loads = [s["loadavg"] for s in ph if s["loadavg"] is not None]
            out[tag]["loadavg_mean"] = (sum(loads) / len(loads)) if loads else None
            # misses del keystore (fibra seca) y ORR send_failed durante la fase
            q_first, q_last = ph[0]["Q"], ph[-1]["Q"]
            out[tag]["keystore_misses_delta"] = sum(fnum(q, "misses") for q in q_last) - sum(fnum(q, "misses") for q in q_first)
            out[tag]["keystore_taken_delta"] = sum(fnum(q, "taken") for q in q_last) - sum(fnum(q, "taken") for q in q_first)
            # QKC dry links: fracción de muestras con enc=0 en algún enlace (aprox por nodo: enc total 0)
        if tag == "L2" and ph:
            # sostenido corregido por stock: ventana = últimos 2/3 de la fase
            persec = read_persec(os.path.join(cdir, "L2.persec.csv"))
            if persec and len(ph) >= 3:
                t0 = ph[0]["t"]
                t_end = ph[-1]["t"]
                w0 = t0 + (t_end - t0) / 3.0
                win = [s for s in ph if s["t"] >= w0 and s["D"]]
                if len(win) >= 2:
                    stock0, stock1 = sum_enc(win[0]), sum_enc(win[-1])
                    # Segundo almacén: los anillos ENC de los QKC (claves QKD ya
                    # sacadas del KME, líneas Q). El tercero, el búfer del propio
                    # quditto (8192 claves por arista), no se muestrea: se da su
                    # cota como residuo (a 300 s de L2 es <10 % del techo).
                    ring0 = sum(fnum(q, "enc") for q in win[0]["Q"])
                    ring1 = sum(fnum(q, "enc") for q in win[-1]["Q"])
                    kme0 = sum(fnum(k, "stored") for k in win[0]["K"])
                    kme1 = sum(fnum(k, "stored") for k in win[-1]["K"])
                    ta, tb = win[0]["t"], win[-1]["t"]
                    served_win = sum(r["ok_keys"] for r in persec if ta <= r["t_unix"] < tb)
                    dur = tb - ta
                    # Los tres almacenes: buffers ENC de los DKMS, anillos ENC de
                    # los QKC y el búfer de cada KME (quditto, muestreado).
                    drained = (stock0 - stock1) + (ring0 - ring1) + (kme0 - kme1)
                    out[tag]["window_s"] = dur
                    out[tag]["served_window_keys_per_s"] = served_win / dur if dur else None
                    out[tag]["stock_drain_keys_per_s"] = (stock0 - stock1) / dur if dur else None
                    out[tag]["ring_drain_keys_per_s"] = (ring0 - ring1) / dur if dur else None
                    out[tag]["kme_drain_keys_per_s"] = (kme0 - kme1) / dur if dur else None
                    out[tag]["kme_sampled"] = bool(win[0]["K"])
                    out[tag]["sustained_corrected_keys_per_s"] = (served_win - drained) / dur if dur else None
                    out[tag]["quditto_stock_bound_keys_per_s"] = None if win[0]["K"] else ((topo["edges"] * 8192.0) / dur if dur else None)
                    out[tag]["stock_start_frac"] = stock0 / cap_stock if cap_stock else None
                    out[tag]["stock_end_frac"] = stock1 / cap_stock if cap_stock else None
                    # Lo que de verdad gasta la fibra: claves QKD sacadas por los
                    # QKC (Σtaken) frente a lo que producen los KME (Σcap), y
                    # cuántas cuesta cada clave de transporte entregada
                    # (saltos EFECTIVOS = Σtaken/Σemitted). El techo Σcap/ħ es
                    # el de demanda UNIFORME; el asignador α-fair favorece a los
                    # pares cortos y el agregado puede superarlo sin que la
                    # fibra dé más de sí (canario N=10: 95 % de utilización,
                    # 1,59 saltos efectivos frente a 2,78 de media).
                    taken0 = sum(fnum(q, "taken") for q in win[0]["Q"])
                    taken1 = sum(fnum(q, "taken") for q in win[-1]["Q"])
                    em0 = sum(fnum(d, "emitted") for d in win[0]["D"])
                    em1 = sum(fnum(d, "emitted") for d in win[-1]["D"])
                    sum_cap = float(topo.get("sum_cap_keys_per_s") or 0.0)
                    out[tag]["fibre_used_keys_per_s"] = (taken1 - taken0) / dur if dur else None
                    out[tag]["fibre_utilisation"] = ((taken1 - taken0) / dur / sum_cap) if (dur and sum_cap) else None
                    out[tag]["transport_emitted_keys_per_s"] = (em1 - em0) / dur if dur else None
                    out[tag]["effective_hops"] = ((taken1 - taken0) / (em1 - em0)) if (em1 - em0) > 0 else None
    # ── REC ──
    rec = phase_samples(samples, "REC")
    if rec:
        pts = [(s["t"], sum_enc(s)) for s in rec if s["D"]]
        out["rec_refill_slope_keys_per_s"] = linear_slope(pts) if len(pts) >= 2 else None
        out["rec_final_stock_frac"] = pts[-1][1] / cap_stock if pts and cap_stock else None
    # ── integridad ──
    for tag in ("L1", "final"):
        r = keys_log_result(os.path.join(cdir, "keys_%s.log" % tag))
        if r:
            out["keys_" + tag] = {"pairs": r[0], "identical": r[1], "failed": r[2]}
    out["l1_offered_total"] = meta.get("l1_offered_total")
    return out


def fmt(v, d=0):
    if v is None:
        return "—"
    if isinstance(v, float):
        return ("%%.%df" % d) % v
    return str(v)


def build_tables(cells):
    """Tablas markdown por métrica: filas N, columnas familias."""
    by = {(c["family"], c["n"]): c for c in cells}

    def table(title, getter, d=0, note=""):
        lines = ["### " + title, ""]
        if note:
            lines += [note, ""]
        lines.append("| N | " + " | ".join(FAM_LABEL[f] for f in FAMILIES) + " |")
        lines.append("|---|" + "---|" * len(FAMILIES))
        for n in NS:
            row = []
            for f in FAMILIES:
                c = by.get((f, n))
                row.append(fmt(getter(c), d) if c else "·")
            lines.append("| %d | " % n + " | ".join(row) + " |")
        lines.append("")
        return "\n".join(lines)

    def g(path, d=None):
        def _get(c):
            cur = c
            for k in path:
                if cur is None:
                    return None
                cur = cur.get(k) if isinstance(cur, dict) else None
            return cur
        return _get

    out = []
    out.append(table("Techo de fibra Σcap/saltos (claves/s)", g(["techo_fibra"])))
    out.append(table("Techo del enlace más cargado (claves/s)", g(["techo_cuello"])))
    out.append(table("Saltos medios (pares ordenados)", g(["mean_hops"]), 2))
    out.append(table("L0 · tiempo hasta todos los pares a tope (s)", g(["t_full_s"]),
                     note="«—» = no llegó a tope dentro de la ventana L0 (600 s)."))
    out.append(table("L0 · pendiente de llenado (claves/s, Σ sobre todos los pares)", g(["l0_fill_slope_keys_per_s"])))
    out.append(table("L0 · tasa medida por el estimador del QKC (media por enlace, claves/s)", g(["l0_estimator_rate_mean"])))
    out.append(table("L1 · ofrecido (claves/s)", g(["l1_offered_total"])))
    out.append(table("L1 · servido (claves/s)", g(["L1", "served_keys_per_s"])))
    out.append(table("L1 · fracción de peticiones rechazadas (429/503)", g(["L1", "reject_frac"]), 3))
    out.append(table("L1 · latencia p50 (ms)", g(["L1", "lat_p50_ms"]), 1))
    out.append(table("L1 · latencia p99 (ms)", g(["L1", "lat_p99_ms"]), 1))
    out.append(table("L2 · servido bruto (claves/s, con stock)", g(["L2", "served_keys_per_s"])))
    out.append(table("L2 · SOSTENIDO corregido por stock (claves/s)", g(["L2", "sustained_corrected_keys_per_s"]),
                     note="(servido − stock drenado en la ventana) / ventana, sobre los últimos 2/3 de L2; el stock son los tres almacenes: buffers ENC de los DKMS, anillos ENC de los QKC y búferes de los KME (quditto, muestreados)."))
    out.append(table("L2 · drenado del KME en la ventana (claves/s)", g(["L2", "kme_drain_keys_per_s"])))
    out.append(table("L2 · sostenido / techo de fibra", lambda c: (c["L2"]["sustained_corrected_keys_per_s"] / c["techo_fibra"]) if c.get("L2", {}).get("sustained_corrected_keys_per_s") is not None and c["techo_fibra"] else None, 2))
    out.append(table("L2 · fibra consumida por los QKC (Σtaken, claves/s)", g(["L2", "fibre_used_keys_per_s"])))
    out.append(table("L2 · utilización de la fibra (Σtaken / Σcap)", g(["L2", "fibre_utilisation"]), 2,
                     note="Cerca de 1 = la fibra manda (el KME no da más); muy por debajo = manda el software o el asignador."))
    out.append(table("L2 · saltos efectivos por clave entregada (Σtaken / Σemitted)", g(["L2", "effective_hops"]), 2,
                     note="Comparar con los saltos medios uniformes: menor = el asignador α-fair favorece a los pares cortos."))
    out.append(table("L2 · índice de Jain de las claves servidas por par", g(["L2", "jain_index"]), 3))
    out.append(table("L2 · peor par / reparto uniforme", g(["L2", "min_pair_share"]), 2))
    out.append(table("L2 · fracción rechazada (429/503)", g(["L2", "reject_frac"]), 3))
    out.append(table("L2 · latencia p50 (ms)", g(["L2", "lat_p50_ms"]), 1))
    out.append(table("L2 · latencia p99 (ms)", g(["L2", "lat_p99_ms"]), 1))
    out.append(table("L2 · ENC vacío (fracción media de pares con enc=0)", g(["L2", "enc_zero_frac_mean"]), 3))
    out.append(table("L2 · pares sin ninguna clave servida", g(["L2", "pairs_zero"])))
    out.append(table("L2 · CPU de los módulos (% acumulado, media; 6400 = nodo entero)",
                     lambda c: sum(v for k, v in c["L2"]["cpu_mean_pct"].items() if k != "sae_load") if c.get("L2", {}).get("cpu_mean_pct") else None))
    out.append(table("L2 · CPU del cliente de carga (%)", lambda c: c["L2"]["cpu_mean_pct"].get("sae_load") if c.get("L2", {}).get("cpu_mean_pct") else None))
    out.append(table("L2 · RSS pico total (MB)", g(["L2", "rss_total_peak_mb"])))
    out.append(table("REC · tiempo de recuperación a tope (s)", g(["t_recover_s"]), note="«—» = no recuperó en 180 s."))
    out.append(table("REC · pendiente de rellenado (claves/s)", g(["rec_refill_slope_keys_per_s"])))
    out.append(table("Salud · recv_corrupt", g(["health", "recv_corrupt"])))
    out.append(table("Salud · peel_failed + dropped_no_secret", lambda c: (c["health"].get("peel_failed", 0) + c["health"].get("dropped_no_secret", 0)) if c.get("health") else None))
    out.append(table("Salud · rechazos del sello por-frame (bad_mac+replayed+plain_rej)", g(["health", "frame_auth_rejects"])))
    out.append(table("Salud · procesos muertos + panics", lambda c: (c["health"].get("dead_processes", 0) + c["health"].get("panics", 0)) if c.get("health") else None))
    out.append(table("Integridad · intercambios ETSI-014 fallidos (L1 + final)", lambda c: (c.get("keys_L1", {}).get("failed", 0) + c.get("keys_final", {}).get("failed", 0)) if (c.get("keys_L1") or c.get("keys_final")) else None))
    out.append(table("Bring-up (s)", g(["t_up_s"])))
    return "\n".join(out)


def make_plots(cells, outdir):
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        print("matplotlib no disponible: sin figuras", file=sys.stderr)
        return []
    by = defaultdict(dict)
    for c in cells:
        by[c["family"]][c["n"]] = c
    colors = {"estrella": "#e6a100", "anillo": "#1f77b4", "puente": "#d62728",
              "malla": "#2ca02c", "rgg": "#9467bd", "aleatoria": "#7f7f7f"}
    figs = []

    def series(getter):
        out = {}
        for f in FAMILIES:
            xs, ys = [], []
            for n in NS:
                c = by[f].get(n)
                if not c:
                    continue
                v = getter(c)
                if v is None:
                    continue
                xs.append(n)
                ys.append(v)
            out[f] = (xs, ys)
        return out

    def plot(name, title, ylabel, getter, logy=False, extra=None):
        fig, ax = plt.subplots(figsize=(8, 4.8))
        for f, (xs, ys) in series(getter).items():
            if xs:
                ax.plot(xs, ys, marker="o", color=colors[f], label=FAM_LABEL[f])
        if extra:
            extra(ax)
        ax.set_xlabel("N (nodos)")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        ax.grid(True, alpha=0.3)
        if logy:
            ax.set_yscale("log")
        if ax.get_legend_handles_labels()[0]:
            ax.legend(fontsize=8)
        fig.tight_layout()
        for ext in ("png", "svg"):
            fig.savefig(os.path.join(outdir, name + "." + ext), dpi=130)
        plt.close(fig)
        figs.append(name)

    plot("l2_sustained", "L2 · sostenido corregido por stock", "claves/s",
         lambda c: c.get("L2", {}).get("sustained_corrected_keys_per_s"))
    plot("l2_vs_ceiling", "L2 · sostenido / techo de fibra", "fracción",
         lambda c: (c["L2"]["sustained_corrected_keys_per_s"] / c["techo_fibra"]) if c.get("L2", {}).get("sustained_corrected_keys_per_s") is not None and c["techo_fibra"] else None)
    plot("ceilings", "Techo de fibra (Σcap/saltos) por topología", "claves/s", lambda c: c["techo_fibra"], logy=True)
    plot("l1_served_vs_offered", "L1 · servido / ofrecido", "fracción",
         lambda c: (c["L1"]["served_keys_per_s"] / c["l1_offered_total"]) if c.get("L1") and c.get("l1_offered_total") else None)
    plot("l1_latency", "L1 · latencia p50 / p99", "ms", lambda c: c.get("L1", {}).get("lat_p99_ms"), logy=True)
    plot("l2_latency_p99", "L2 · latencia p99", "ms", lambda c: c.get("L2", {}).get("lat_p99_ms"), logy=True)
    plot("l0_fill_time", "L0 · tiempo hasta todos los buffers a tope", "s", lambda c: c.get("t_full_s"))
    plot("l0_fill_slope", "L0 · pendiente de llenado (Σ todos los pares)", "claves/s", lambda c: c.get("l0_fill_slope_keys_per_s"))
    plot("l2_reject", "L2 · fracción de peticiones rechazadas (contrapresión)", "fracción", lambda c: c.get("L2", {}).get("reject_frac"))
    plot("l2_fibre_utilisation", "L2 · utilización de la fibra (Σtaken/Σcap)", "fracción", lambda c: c.get("L2", {}).get("fibre_utilisation"))
    plot("l2_effective_hops", "L2 · saltos efectivos por clave entregada", "saltos", lambda c: c.get("L2", {}).get("effective_hops"))
    plot("l2_jain", "L2 · equidad entre pares (índice de Jain)", "Jain", lambda c: c.get("L2", {}).get("jain_index"))
    plot("l2_min_pair_share", "L2 · peor par / reparto uniforme", "fracción", lambda c: c.get("L2", {}).get("min_pair_share"))
    plot("l2_cpu_modules", "L2 · CPU de los módulos (6400 % = nodo entero)", "% acumulado",
         lambda c: sum(v for k, v in c["L2"]["cpu_mean_pct"].items() if k != "sae_load") if c.get("L2", {}).get("cpu_mean_pct") else None)
    plot("rss_peak", "L2 · RSS pico total", "MB", lambda c: c.get("L2", {}).get("rss_total_peak_mb"))
    plot("rec_time", "REC · recuperación a tope tras L2", "s", lambda c: c.get("t_recover_s"))
    plot("hops", "Saltos medios por topología", "saltos", lambda c: c["mean_hops"])
    return figs


def draw_topologies(outdir, n=20):
    """Dibuja las 6 familias a N=20 (figs/topo_<fam>.svg) con las aristas
    coloreadas por capacidad (solo varía en la RGG)."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        import importlib.util
    except ImportError:
        return
    spec = importlib.util.spec_from_file_location("topologies", os.path.join(os.path.dirname(__file__), "topologies.py"))
    tp = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(tp)
    for fam in FAMILIES:
        if fam == "estrella":
            es = tp.fam_estrella(n, 5.0)
            pos = {1: (0.0, 0.0)}
            for k in range(2, n + 1):
                a = 2 * math.pi * (k - 2) / (n - 1)
                pos[k] = (math.cos(a), math.sin(a))
        elif fam == "anillo":
            es = tp.fam_anillo(n, 5.0)
            pos = {k: (math.cos(2 * math.pi * k / n), math.sin(2 * math.pi * k / n)) for k in range(1, n + 1)}
        elif fam == "puente":
            es = tp.fam_puente(n, 5.0)
            h = n // 2
            pos = {}
            for k in range(1, h + 1):
                a = 2 * math.pi * k / h
                pos[k] = (-1.3 + 0.9 * math.cos(a), 0.9 * math.sin(a))
            for k in range(h + 1, n + 1):
                a = 2 * math.pi * (k - h) / h
                pos[k] = (1.3 + 0.9 * math.cos(a + math.pi), 0.9 * math.sin(a + math.pi))
        elif fam == "malla":
            es = tp.fam_malla(n, 5.0)
            best = max(r for r in range(1, int(math.sqrt(n)) + 1) if n % r == 0)
            r, c = best, n // best
            pos = {1 + i * c + j: (j / max(1, c - 1) * 2 - 1, -(i / max(1, r - 1) * 2 - 1)) for i in range(r) for j in range(c)}
        elif fam == "rgg":
            es, pts = tp.fam_rgg(n, 30.0, 4.0, 42)
            side = 30.0 * math.sqrt(math.pi * n / 4.0)
            pos = {i + 1: (x / side * 2 - 1, y / side * 2 - 1) for i, (x, y) in enumerate(pts)}
        else:
            es = tp.fam_aleatoria(n, 5.0, 42)
            pos = {k: (math.cos(2 * math.pi * k / n), math.sin(2 * math.pi * k / n)) for k in range(1, n + 1)}
        fig, ax = plt.subplots(figsize=(3.4, 3.2))
        caps = {e: tp.cap_keys_per_s(2000.0, 0.2, d) for e, d in es.items()}
        cmax = max(caps.values()) or 1.0
        for (a, b), cap in caps.items():
            (x1, y1), (x2, y2) = pos[a], pos[b]
            ax.plot([x1, x2], [y1, y2], color="#38d3f0", alpha=0.25 + 0.75 * cap / cmax, linewidth=0.8 + 1.6 * cap / cmax)
        xs = [pos[k][0] for k in pos]
        ys = [pos[k][1] for k in pos]
        ax.scatter(xs, ys, s=18, color="#e6ebf4", zorder=3)
        ax.set_aspect("equal")
        ax.axis("off")
        fig.patch.set_alpha(0.0)
        fig.tight_layout(pad=0.2)
        fig.savefig(os.path.join(outdir, "topo_%s.svg" % fam), transparent=True)
        plt.close(fig)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cells", default=os.path.join(os.path.dirname(__file__), "..", "results", "campaign-2026-09", "cells"))
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    cells_dir = os.path.abspath(a.cells)
    outdir = os.path.abspath(a.out or os.path.join(cells_dir, ".."))
    os.makedirs(os.path.join(outdir, "figs"), exist_ok=True)
    cells = []
    for d in sorted(glob.glob(os.path.join(cells_dir, "*-n*"))):
        if not os.path.isdir(d) or not os.path.exists(os.path.join(d, "topology.json")):
            continue
        try:
            cells.append(analyze_cell(d))
        except Exception as e:  # noqa: BLE001
            print("celda %s: %s" % (os.path.basename(d), e), file=sys.stderr)
    cells.sort(key=lambda c: (FAMILIES.index(c["family"]) if c["family"] in FAMILIES else 99, c["n"]))
    json.dump(cells, open(os.path.join(outdir, "metrics.json"), "w"), indent=1, sort_keys=True)
    tables = build_tables(cells)
    open(os.path.join(outdir, "TABLES.md"), "w").write("# Tablas de la campaña 2026-09\n\n" + tables)
    figs = make_plots(cells, os.path.join(outdir, "figs"))
    draw_topologies(os.path.join(outdir, "figs"))
    done = sum(1 for c in cells if c["done"])
    print("celdas analizadas: %d (DONE: %d, FAILED: %d); figuras: %d; salida en %s" % (
        len(cells), done, sum(1 for c in cells if c["failed"]), len(figs), outdir))


if __name__ == "__main__":
    main()
