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


ANSI = re.compile(r"\x1b\[[0-9;]*m")


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


# Fallos del CLIENTE de la ronda, no del sistema: curl no arranca (en CESGA
# su OpenSSL 1.1.1g no carga un cert cliente ML-DSA), no resuelve, no conecta.
# Decir «bytes distintos» de esto sería mentir sobre la integridad.
CLIENT_ERR = re.compile(
    r"curl: \(\d+\)|could not load PEM|SSL certificate problem|Connection refused"
    r"|Could not resolve|Failed to connect|Operation timed out|Empty reply",
    re.I,
)


def keys_log_result(path):
    """`mesh.sh keys` → (pares, idénticos, fallidos, throttled, mismatch,
    client_error).  Un «fallido» por 429/503 es contrapresión (bucket por SAE
    agotado, buffer vacío) y uno de `CLIENT_ERR` es del arnés: ninguno es un
    fallo de integridad. Solo cuenta como tal lo que no es ni una cosa ni la
    otra (bytes distintos, error de protocolo del servidor)."""
    if not os.path.exists(path):
        return None
    last = ""
    throttled = 0
    other = 0
    client = 0
    with open(path, errors="replace") as fh:
        for line in fh:
            if "pares ordenados" in line:
                last = line
            elif line.lstrip().startswith("✗"):
                if "429" in line or "503" in line:
                    throttled += 1
                elif CLIENT_ERR.search(line):
                    client += 1
                else:
                    other += 1
    m = re.search(r"pares ordenados:\s*(\d+)\s+idénticos:\s*(\d+)\s+fallidos:\s*(\d+)", last)
    if not m:
        return None
    return tuple(int(x) for x in m.groups()) + (throttled, other, client)


INCLUDE_PARTIAL = False
# Modelo del enlace QKD de la campaña (mesh.sh: DKMS_MESH_R0 / _ALPHA / _DIST_KM)
QKD_R0 = 2000.0
QKD_ALPHA = 0.2
QKD_DIST_KM = 5.0


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
    # Frames descartados en la cola de entrada acotada del QKC (INTAKE_QUEUE):
    # solo se conservan los logs de qkc1 y qkc2, y la línea se emite en
    # potencias de dos (nth_is_loud), así que es una cota INFERIOR (≥). En la
    # estrella qkc1 es el hub, que es donde importa.
    intake_min = 0
    for q in ("qkc1", "qkc2"):
        lp = os.path.join(cdir, q + ".log")
        if not (os.path.exists(lp) or os.path.exists(lp + ".gz")):
            continue
        try:
            with open_maybe_gz(lp) as fh:
                for line in fh:
                    if "intake_full" not in line:
                        continue
                    mm = re.search(r"total\S*?=\S*?(\d+)", ANSI.sub("", line))
                    if mm:
                        intake_min = max(intake_min, int(mm.group(1)))
        except (OSError, EOFError):
            pass
    out["health"] = dict(out["health"] or {}, intake_dropped_frames_min=intake_min)
    # Distancias de las aristas (edges.tsv: idx a b km) y su capacidad con el
    # modelo de la campaña (R0=2000, α=0,2): uniforme en cinco familias,
    # geométrica en la RGG. Es lo que fija todo techo de esta página.
    ep = os.path.join(cdir, "edges.tsv")
    if os.path.exists(ep):
        dk = []
        for line in open(ep):
            parts = line.split()
            if len(parts) >= 4:
                try:
                    dk.append(float(parts[3]))
                except ValueError:
                    pass
        if dk:
            dk.sort()
            caps = [QKD_R0 * 10 ** (-QKD_ALPHA * d / 10.0) for d in dk]
            out["dist_km"] = {"min": dk[0], "median": dk[len(dk) // 2], "mean": sum(dk) / len(dk), "max": dk[-1]}
            out["link_cap"] = {"min": min(caps), "median": sorted(caps)[len(caps) // 2], "mean": sum(caps) / len(caps), "max": max(caps),
                               "sum": sum(caps)}
            out["edge_km"] = [round(d, 2) for d in dk]
    # Una celda sin DONE (corriendo, o muerta a medias) aporta su teoría (la
    # topología existe desde el arranque) pero NO métricas medidas: a medias
    # serían un punto falso en las tablas y figuras.
    if not out["done"] and not INCLUDE_PARTIAL:
        return out
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
        # Curva de llenado (fracción del stock total frente a segundos desde el
        # arranque), ≤ 80 puntos: es lo que dibuja `fill_<familia>` en la web.
        curve = [(s["up"], sum_enc(s) / cap_stock) for s in l0 if s["D"] and cap_stock]
        step = max(1, len(curve) // 80)
        out["l0_curve"] = [[int(t), round(v, 4)] for t, v in curve[::step]]
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
                # Cuantiles del reparto por par (claves del par / media), 101
                # puntos: la CDF de equidad de la web sin arrastrar N·(N−1) filas.
                if mean > 0:
                    sv = sorted(v / mean for v in vals)
                    out[tag]["pair_share_quantiles"] = [round(sv[min(len(sv) - 1, int(q * (len(sv) - 1) / 100))], 4) for q in range(101)]
        # Serie temporal de la carga en cajas de 10 s (servido/s y rechazos/s),
        # relativa al primer segundo de L1 de la celda (`t_l1_start`).
        persec_rows = read_persec(os.path.join(cdir, tag + ".persec.csv"))
        if persec_rows:
            t0 = min(r["t_unix"] for r in persec_rows)
            if tag == "L1":
                out["t_l1_start"] = t0
            base = out.get("t_l1_start", t0)
            bins = defaultdict(lambda: [0.0, 0.0, 0])
            for r in persec_rows:
                b = int((r["t_unix"] - base) // 10)
                bins[b][0] += r["ok_keys"]
                bins[b][1] += r["n429"] + r["n503"] + r["n_other"]
                bins[b][2] += 1
            out[tag]["timeline"] = [[b * 10, round(v[0] / 10.0, 1), round(v[1] / 10.0, 1)] for b, v in sorted(bins.items()) if v[2] > 0]
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
            if persec and len(ph) >= 2:
                t0 = ph[0]["t"]
                t_end = ph[-1]["t"]
                w0 = t0 + (t_end - t0) / 3.0
                win = [s for s in ph if s["t"] >= w0 and s["D"]]
                # A N≥80 en las familias densas el muestreador (que compite con
                # miles de flujos en bucle cerrado) deja 2-4 muestras en toda
                # la fase: si en los últimos 2/3 no caen dos, se usan las dos
                # últimas de la fase (≥ 60 s de separación) y se marca.
                relaxed = False
                if len(win) < 2:
                    alld = [s for s in ph if s["D"]]
                    if len(alld) >= 2 and (alld[-1]["t"] - alld[-2]["t"]) >= 60:
                        win = alld[-2:]
                        relaxed = True
                out[tag]["n_samples"] = len(ph)
                out[tag]["window_relaxed"] = relaxed
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
    # Stock total (fracción) a lo largo de L1 → L2 → REC, relativo al inicio
    # de L1, para superponerlo a la serie de carga en `timeline_<familia>`.
    if out.get("t_l1_start"):
        base = out["t_l1_start"]
        st = [(s["t"] - base, sum_enc(s) / cap_stock, s["phase"]) for s in samples
              if s["phase"] in ("L1", "L2", "REC") and s["D"] and cap_stock]
        out["stock_timeline"] = [[int(t), round(v, 4), ph] for t, v, ph in st]
    # ── integridad ──
    for tag in ("L1", "final"):
        r = keys_log_result(os.path.join(cdir, "keys_%s.log" % tag))
        if r:
            out["keys_" + tag] = {"pairs": r[0], "identical": r[1], "failed": r[2],
                                  "throttled": r[3], "mismatch_or_other": r[4],
                                  "client_error": r[5]}
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
    out.append(table("L2 · muestras del muestreador en la fase (ventana relajada a 2 muestras si <2 en los últimos 2/3)", lambda c: ("%d%s" % (c["L2"]["n_samples"], "*" if c["L2"].get("window_relaxed") else "")) if c.get("L2", {}).get("n_samples") else None))
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
    out.append(table("Integridad · intercambios ETSI-014 con bytes distintos u otro error (L1 + final)", lambda c: (c.get("keys_L1", {}).get("mismatch_or_other", 0) + c.get("keys_final", {}).get("mismatch_or_other", 0)) if (c.get("keys_L1") or c.get("keys_final")) else None,
                     note="Los 429/503 de una ronda (contrapresión, buffer vacío) van en la tabla siguiente: no son fallos de integridad."))
    out.append(table("Integridad · intercambios rechazados por 429/503 en las rondas (L1 + final)", lambda c: (c.get("keys_L1", {}).get("throttled", 0) + c.get("keys_final", {}).get("throttled", 0)) if (c.get("keys_L1") or c.get("keys_final")) else None))
    out.append(table("Integridad · rondas no ejecutadas por fallo del CLIENTE (curl sin ML-DSA, etc.)", lambda c: (c.get("keys_L1", {}).get("client_error", 0) + c.get("keys_final", {}).get("client_error", 0)) if (c.get("keys_L1") or c.get("keys_final")) else None,
                     note="No es un resultado del sistema: el cliente de la ronda no llegó a preguntar."))
    out.append(table("Salud · claves expiradas por ACK (emitidas sin ACK en 30 s; material descartado por el emisor)", g(["health", "expired"]),
                     note="En la estrella crece con N desde N≈40: la cola de entrada acotada del QKC del hub (8192 frames) desborda al arrancar L2 y los frames descartados nunca se ACKean."))
    out.append(table("Salud · frames descartados en la cola de entrada del QKC (≥; solo nodos 1-2 conservan log; hub = nodo 1 en la estrella)", g(["health", "intake_dropped_frames_min"])))
    out.append(table("Bring-up (s)", g(["t_up_s"])))
    return "\n".join(out)


FAM_LABEL_EN = {"estrella": "Star", "anillo": "Ring (C_N)", "puente": "Bridge",
                "malla": "Mesh (grid)", "rgg": "RGG (QKD at distance)", "aleatoria": "Random"}
# (título, etiqueta y) en inglés por nombre de figura; las unidades compartidas
# (s, ms, MB, Jain) se dejan igual.
PLOT_EN = {
    "l2_sustained": ("L2 · sustained, stock-corrected", "keys/s"),
    "l2_vs_ceiling": ("L2 · sustained / fibre ceiling", "fraction"),
    "ceilings": ("Fibre ceiling (Σcap/hops) per topology", "keys/s"),
    "l1_served_vs_offered": ("L1 · served / offered", "fraction"),
    "l1_latency": ("L1 · latency p99", "ms"),
    "l2_latency_p99": ("L2 · latency p99", "ms"),
    "l0_fill_time": ("L0 · time until every buffer is full", "s"),
    "l0_fill_slope": ("L0 · fill slope (Σ over all pairs)", "keys/s"),
    "l2_reject": ("L2 · fraction of rejected requests (back-pressure)", "fraction"),
    "l2_fibre_utilisation": ("L2 · fibre utilisation (Σtaken/Σcap)", "fraction"),
    "l2_effective_hops": ("L2 · effective hops per delivered key", "hops"),
    "l2_jain": ("L2 · fairness between pairs (Jain index)", "Jain"),
    "l2_min_pair_share": ("L2 · worst pair / uniform share", "fraction"),
    "l2_cpu_modules": ("L2 · CPU of the modules (6400 % = whole node)", "% cumulative"),
    "rss_peak": ("L2 · total peak RSS", "MB"),
    "rec_time": ("REC · recovery to full after L2", "s"),
    "hops": ("Mean hops per topology", "hops"),
}


def make_plots(cells, outdir, lang="es"):
    os.makedirs(outdir, exist_ok=True)
    fam_label = FAM_LABEL if lang == "es" else FAM_LABEL_EN
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
    # Paleta y fondo de la web (las figuras se incrustan como SVG en una
    # página oscura: fondo transparente, texto y rejilla apagados).
    colors = {"estrella": "#e6a100", "anillo": "#38d3f0", "puente": "#ff6b6b",
              "malla": "#4cd97b", "rgg": "#9d7bfa", "aleatoria": "#93a1bb"}
    ink, muted, grid = "#e6ebf4", "#93a1bb", "#2c3a5c"
    plt.rcParams.update({
        "figure.facecolor": "none", "axes.facecolor": "none", "savefig.facecolor": "none",
        "axes.edgecolor": grid, "axes.labelcolor": muted, "axes.titlecolor": ink,
        "xtick.color": muted, "ytick.color": muted, "grid.color": grid,
        "text.color": ink, "legend.facecolor": "#101829", "legend.edgecolor": grid,
        "legend.labelcolor": ink, "font.family": "DejaVu Sans", "font.size": 11,
        "axes.titlesize": 12, "legend.fontsize": 9.5,
        "axes.spines.top": False, "axes.spines.right": False,
    })
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
                ax.plot(xs, ys, marker="o", color=colors[f], label=fam_label[f])
        if extra:
            extra(ax)
        if lang != "es":
            title, ylabel = PLOT_EN.get(name, (title, ylabel))
        ax.set_xlabel("N (nodos)" if lang == "es" else "N (nodes)")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        ax.grid(True, alpha=0.6, linewidth=0.6)
        if logy:
            ax.set_yscale("log")
        if ax.get_legend_handles_labels()[0]:
            ax.legend()
        fig.tight_layout()
        # SVG transparente (va incrustado en la web); PNG con el fondo oscuro
        # de la web para que se lea en el ANALYSIS.md.
        fig.savefig(os.path.join(outdir, name + ".svg"), transparent=True)
        fig.savefig(os.path.join(outdir, name + ".png"), dpi=130, facecolor="#101829")
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


EXTRA_EN = {
    "fill": ("L0 · fill from empty — %s", "stock (%% of N·(N−1)·4096 keys)", "seconds since bring-up"),
    "timeline": ("L1 → L2 → REC — %s, N=%d", "keys/s", "seconds since L1 start", "stock (%)", "served", "rejected (429/503)", "stock"),
    "fairness": ("L2 · per-pair share of the served keys, N=%d", "fraction of pairs ≤ x", "keys of the pair / uniform share"),
    "replay_fix": ("Per-frame seal · legitimate frames rejected as replays", "rejections at node 1", ("before (d60714a)", "after (d40d2fc)")),
    "star_hub": ("Star · what the hub costs as N grows", "count (log)", ("keys expired at senders", "frames dropped by the hub's intake (≥)")),
    "estimator": ("L0 · in-situ QKD rate estimator / quditto formula", "ratio", "N (nodes)"),
    "l2_latency_p50": ("L2 · latency p50", "ms"), "l1_latency_p50": ("L1 · latency p50", "ms"),
    "bringup": ("Bring-up time of the whole cell", "s"), "l0_pairs_full": ("L0 · pairs full at the end of the window", "fraction"),
    "l2_pairs_zero": ("L2 · ordered pairs with no key served", "pairs"),
}
EXTRA_ES = {
    "fill": ("L0 · llenado desde vacío — %s", "stock (%% de N·(N−1)·4096 claves)", "segundos desde el arranque"),
    "timeline": ("L1 → L2 → REC — %s, N=%d", "claves/s", "segundos desde el inicio de L1", "stock (%)", "servido", "rechazado (429/503)", "stock"),
    "fairness": ("L2 · reparto por par de las claves servidas, N=%d", "fracción de pares ≤ x", "claves del par / reparto uniforme"),
    "replay_fix": ("Sello por frame · frames legítimos rechazados como repetidos", "rechazos en el nodo 1", ("antes (d60714a)", "después (d40d2fc)")),
    "star_hub": ("Estrella · lo que cuesta el hub al crecer N", "cuenta (log)", ("claves expiradas en los emisores", "frames descartados en la cola del hub (≥)")),
    "estimator": ("L0 · estimador in situ de tasa QKD / fórmula de quditto", "cociente", "N (nodos)"),
    "l2_latency_p50": ("L2 · latencia p50", "ms"), "l1_latency_p50": ("L1 · latencia p50", "ms"),
    "bringup": ("Tiempo de arranque de la celda entera", "s"), "l0_pairs_full": ("L0 · pares a tope al cerrar la ventana", "fracción"),
    "l2_pairs_zero": ("L2 · pares ordenados sin ninguna clave servida", "pares"),
}


def make_extra_plots(cells, outdir, lang="es", rerun=None):
    """Figuras de detalle para la web: curvas de llenado y líneas temporales por
    familia, CDF de equidad, antes/después del fix del replay, el hub de la
    estrella, el estimador, latencias p50 y arranque."""
    try:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
    except ImportError:
        return []
    os.makedirs(outdir, exist_ok=True)
    tx = EXTRA_ES if lang == "es" else EXTRA_EN
    fam_label = FAM_LABEL if lang == "es" else FAM_LABEL_EN
    colors = {"estrella": "#e6a100", "anillo": "#38d3f0", "puente": "#ff6b6b",
              "malla": "#4cd97b", "rgg": "#9d7bfa", "aleatoria": "#93a1bb"}
    by = defaultdict(dict)
    for c in cells:
        by[c["family"]][c["n"]] = c
    figs = []

    def save(fig, name):
        fig.tight_layout()
        fig.savefig(os.path.join(outdir, name + ".svg"), transparent=True)
        fig.savefig(os.path.join(outdir, name + ".png"), dpi=130, facecolor="#101829")
        plt.close(fig)
        figs.append(name)

    # gradiente por N (frío → cálido) sobre fondo oscuro
    n_col = {n: plt.cm.plasma(0.15 + 0.75 * (i / (len(NS) - 1))) for i, n in enumerate(NS)}

    for f in FAMILIES:
        # ── curvas de llenado ──
        fig, ax = plt.subplots(figsize=(8, 4.8))
        for n in NS:
            c = by[f].get(n)
            if not c or not c.get("l0_curve"):
                continue
            xs = [p[0] for p in c["l0_curve"]]
            ys = [100 * p[1] for p in c["l0_curve"]]
            ax.plot(xs, ys, color=n_col[n], label="N=%d" % n, linewidth=1.4)
        ax.axhline(100, color="#5d6b87", linestyle=":", linewidth=0.8)
        ax.set_ylim(0, 105)
        ax.set_xlabel(tx["fill"][2])
        ax.set_ylabel(tx["fill"][1].replace("%%", "%"))
        ax.set_title(tx["fill"][0] % fam_label[f])
        ax.grid(True, alpha=0.6, linewidth=0.6)
        ax.legend(ncol=2)
        save(fig, "fill_%s" % f)
        # ── línea temporal a N=50 y N=100 ──
        for n in (50, 100):
            c = by[f].get(n)
            if not c or not (c.get("L1", {}).get("timeline") or c.get("L2", {}).get("timeline")):
                continue
            fig, ax = plt.subplots(figsize=(8, 4.8))
            ax2 = ax.twinx()
            for tag, alpha in (("L1", 0.9), ("L2", 0.9)):
                tl = c.get(tag, {}).get("timeline") or []
                if tl:
                    ax.plot([p[0] for p in tl], [p[1] for p in tl], color=colors[f], linewidth=1.4,
                            label=tx["timeline"][4] if tag == "L1" else None)
                    ax.plot([p[0] for p in tl], [p[2] for p in tl], color="#ff6b6b", linewidth=1.0, linestyle="--",
                            label=tx["timeline"][5] if tag == "L1" else None)
            st = c.get("stock_timeline") or []
            if st:
                ax2.plot([p[0] for p in st], [100 * p[1] for p in st], color="#e6ebf4", linewidth=1.2, linestyle=":", label=tx["timeline"][6])
                ax2.set_ylim(0, 105)
                ax2.set_ylabel(tx["timeline"][3])
                ax2.tick_params(colors="#93a1bb")
                # sombreado de fases según el stock (que cubre L1, L2 y REC)
                spans = {}
                for t, _, ph in st:
                    spans.setdefault(ph, [t, t])
                    spans[ph][1] = t
                for ph, (a, b) in spans.items():
                    ax.axvspan(a, b, color={"L1": "#38d3f0", "L2": "#ff6b6b", "REC": "#4cd97b"}.get(ph, "#93a1bb"), alpha=0.06)
                    ax.text((a + b) / 2, ax.get_ylim()[1] * 0.98, ph, ha="center", va="top", fontsize=9, color="#93a1bb")
            ax.set_xlabel(tx["timeline"][2])
            ax.set_ylabel(tx["timeline"][1])
            ax.set_title(tx["timeline"][0] % (fam_label[f], n))
            ax.grid(True, alpha=0.6, linewidth=0.6)
            h1, l1 = ax.get_legend_handles_labels()
            h2, l2 = ax2.get_legend_handles_labels()
            ax.legend(h1 + h2, l1 + l2, loc="center right")
            save(fig, "timeline_%s_n%d" % (f, n))

    # ── CDF de equidad ──
    for n in (50, 100):
        fig, ax = plt.subplots(figsize=(8, 4.8))
        any_ = False
        for f in FAMILIES:
            c = by[f].get(n)
            q = (c or {}).get("L2", {}).get("pair_share_quantiles")
            if not q:
                continue
            any_ = True
            ax.plot([max(v, 1e-3) for v in q], [i / 100 for i in range(101)], color=colors[f], label=fam_label[f], linewidth=1.5)
        if any_:
            ax.axvline(1.0, color="#5d6b87", linestyle=":", linewidth=0.8)
            ax.set_xscale("log")
            ax.set_xlabel(tx["fairness"][2])
            ax.set_ylabel(tx["fairness"][1])
            ax.set_title(tx["fairness"][0] % n)
            ax.grid(True, alpha=0.6, linewidth=0.6, which="both")
            ax.legend()
            save(fig, "fairness_cdf_n%d" % n)
        else:
            plt.close(fig)

    # ── antes / después del fix del replay ──
    if rerun:
        fig, ax = plt.subplots(figsize=(8, 4.8))
        names = sorted(rerun.keys(), key=lambda k: (FAMILIES.index(k.rsplit("-n", 1)[0]), int(k.rsplit("-n", 1)[1])))
        xs = range(len(names))
        before = [rerun[k]["before"] for k in names]
        after = [rerun[k]["after"] for k in names]
        ax.bar([x - 0.2 for x in xs], before, width=0.4, color="#ff6b6b", label=tx["replay_fix"][2][0])
        ax.bar([x + 0.2 for x in xs], after, width=0.4, color="#4cd97b", label=tx["replay_fix"][2][1])
        for x, b, a in zip(xs, before, after):
            ax.text(x - 0.2, b, "{:,}".format(b).replace(",", " "), ha="center", va="bottom", fontsize=9)
            ax.text(x + 0.2, max(a, 1), str(a), ha="center", va="bottom", fontsize=9)
        ax.set_xticks(list(xs))
        ax.set_xticklabels([rerun[k]["label"][lang] for k in names])
        ax.set_ylabel(tx["replay_fix"][1])
        ax.set_title(tx["replay_fix"][0])
        ax.grid(True, axis="y", alpha=0.6, linewidth=0.6)
        ax.legend()
        save(fig, "replay_fix")

    # ── el modelo del enlace QKD: cap(d) con R0 y α de la campaña + aristas RGG ──
    fig, ax = plt.subplots(figsize=(8, 4.8))
    ds = [i / 2.0 for i in range(0, 121)]
    ax.plot(ds, [QKD_R0 * 10 ** (-QKD_ALPHA * d / 10.0) for d in ds], color="#e6ebf4", linewidth=1.6,
            label=("R₀·10^(−αd/10), R₀=%d claves/s, α=%.1f dB/km" if lang == "es" else "R₀·10^(−αd/10), R₀=%d keys/s, α=%.1f dB/km") % (QKD_R0, QKD_ALPHA))
    rgg_d = []
    for n in NS:
        c = by["rgg"].get(n)
        if c and c.get("edge_km"):
            rgg_d += c["edge_km"]
    if rgg_d:
        ax2 = ax.twinx()
        ax2.hist(rgg_d, bins=24, range=(0, 60), color="#9d7bfa", alpha=0.35,
                 label=("aristas de la RGG, N=10…100 (%d)" if lang == "es" else "RGG edges, N=10…100 (%d)") % len(rgg_d))
        ax2.set_ylabel("aristas" if lang == "es" else "edges")
        ax2.tick_params(colors="#93a1bb")
        h2, l2 = ax2.get_legend_handles_labels()
    else:
        h2, l2 = [], []
    ax.axvline(QKD_DIST_KM, color="#38d3f0", linestyle="--", linewidth=1.0)
    ax.annotate(("d=5 km: 1 588,7 claves/s\n(estrella, anillo, puente,\nmalla, aleatoria)" if lang == "es" else "d=5 km: 1 588.7 keys/s\n(star, ring, bridge,\nmesh, random)"),
                xy=(QKD_DIST_KM, 1588.7), xytext=(9, 1750), fontsize=9, color="#38d3f0",
                arrowprops={"arrowstyle": "-", "color": "#38d3f0", "linewidth": 0.8})
    for d in (10, 20, 30, 47):
        capd = QKD_R0 * 10 ** (-QKD_ALPHA * d / 10.0)
        ax.plot([d], [capd], marker="o", color="#e6ebf4", markersize=4)
        ax.annotate("%d km: %d" % (d, round(capd)), xy=(d, capd), xytext=(d + 1.2, capd + 90), fontsize=8.5, color="#93a1bb")
    ax.set_xlim(0, 60)
    ax.set_ylim(0, 2100)
    ax.set_xlabel("distancia del enlace (km)" if lang == "es" else "link distance (km)")
    ax.set_ylabel("claves/s por enlace" if lang == "es" else "keys/s per link")
    ax.set_title("El modelo del enlace QKD de la campaña" if lang == "es" else "The campaign's QKD link model")
    ax.grid(True, alpha=0.6, linewidth=0.6)
    h1, l1 = ax.get_legend_handles_labels()
    ax.legend(h1 + h2, l1 + l2, loc="upper right", fontsize=9)
    save(fig, "qkd_model")

    # ── el hub de la estrella ──
    fig, ax = plt.subplots(figsize=(8, 4.8))
    xs = [n for n in NS if n in by["estrella"]]
    exp = [max(1, (by["estrella"][n].get("health") or {}).get("expired", 0)) for n in xs]
    intk = [max(1, (by["estrella"][n].get("health") or {}).get("intake_dropped_frames_min", 0)) for n in xs]
    ax.plot(xs, exp, marker="o", color="#e6a100", label=tx["star_hub"][2][0])
    ax.plot(xs, intk, marker="s", color="#ff6b6b", linestyle="--", label=tx["star_hub"][2][1])
    ax.set_yscale("log")
    ax.set_xlabel("N (nodos)" if lang == "es" else "N (nodes)")
    ax.set_ylabel(tx["star_hub"][1])
    ax.set_title(tx["star_hub"][0])
    ax.grid(True, alpha=0.6, linewidth=0.6, which="both")
    ax.legend()
    save(fig, "star_hub")

    # ── estimador / fórmula, latencias p50, arranque, pares a tope, pares a cero ──
    def simple(name, title, ylabel, getter, logy=False):
        fig, ax = plt.subplots(figsize=(8, 4.8))
        for f in FAMILIES:
            pts = [(n, getter(by[f][n])) for n in NS if n in by[f]]
            pts = [(n, v) for n, v in pts if v is not None]
            if pts:
                ax.plot([p[0] for p in pts], [p[1] for p in pts], marker="o", color=colors[f], label=fam_label[f])
        ax.set_xlabel("N (nodos)" if lang == "es" else "N (nodes)")
        ax.set_ylabel(ylabel)
        ax.set_title(title)
        if logy:
            ax.set_yscale("log")
        ax.grid(True, alpha=0.6, linewidth=0.6)
        if ax.get_legend_handles_labels()[0]:
            ax.legend()
        save(fig, name)

    def est_ratio(c):
        r = c.get("l0_estimator_rate_mean")
        edges = c.get("edges") or 0
        sigma_cap = (c.get("techo_fibra") or 0) * (c.get("mean_hops") or 0)
        return (r / (sigma_cap / edges)) if (r and edges and sigma_cap) else None
    simple("estimator", tx["estimator"][0], tx["estimator"][1], est_ratio)
    simple("l2_latency_p50", tx["l2_latency_p50"][0], tx["l2_latency_p50"][1], lambda c: c.get("L2", {}).get("lat_p50_ms"), logy=True)
    simple("l1_latency_p50", tx["l1_latency_p50"][0], tx["l1_latency_p50"][1], lambda c: c.get("L1", {}).get("lat_p50_ms"), logy=True)
    simple("bringup", tx["bringup"][0], tx["bringup"][1], lambda c: c.get("t_up_s"))
    simple("l0_pairs_full", tx["l0_pairs_full"][0], tx["l0_pairs_full"][1], lambda c: c.get("l0_pairs_full_frac"))
    simple("l2_pairs_zero", tx["l2_pairs_zero"][0], tx["l2_pairs_zero"][1], lambda c: c.get("L2", {}).get("pairs_zero"))
    return figs


def replay_before_after(cells_dir):
    """Rechazos «replayed» en el nodo 1 de las celdas remedidas: antes (copia
    en rerun-prefix/<celda>-d60714a) y después (cells/<celda>)."""
    prefix = os.path.join(cells_dir, "..", "rerun-prefix")
    if not os.path.isdir(prefix):
        return None

    def replayed(logpath):
        last = {}
        if not (os.path.exists(logpath) or os.path.exists(logpath + ".gz")):
            return None
        with open_maybe_gz(logpath) as fh:
            for line in fh:
                if "qkc.frame_auth" not in line or "bad_mac" not in line:
                    continue
                line = ANSI.sub("", line)
                m = re.search(r"peer=(\d+)", line)
                if m:
                    last[m.group(1)] = line
        tot = 0
        for line in last.values():
            m = re.search(r"replayed=(\d+)", line)
            tot += int(m.group(1)) if m else 0
        return tot

    out = {}
    for d in sorted(glob.glob(os.path.join(prefix, "*-d60714a"))):
        cell = os.path.basename(d)[: -len("-d60714a")]
        b = replayed(os.path.join(d, "qkc1.log"))
        a = replayed(os.path.join(cells_dir, cell, "qkc1.log"))
        if b is None or a is None:
            continue
        fam, n = cell.rsplit("-n", 1)
        out[cell] = {"before": b, "after": a,
                     "label": {"es": "%s N=%s" % (FAM_LABEL[fam].split(" ")[0], n), "en": "%s N=%s" % (FAM_LABEL_EN[fam].split(" ")[0], n)}}
    return out or None


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
    ap.add_argument("--partial", action="store_true", help="medir también las celdas sin DONE (corriendo)")
    a = ap.parse_args()
    global INCLUDE_PARTIAL
    INCLUDE_PARTIAL = a.partial
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
    make_plots(cells, os.path.join(outdir, "figs", "en"), lang="en")
    rerun = replay_before_after(cells_dir)
    figs += make_extra_plots(cells, os.path.join(outdir, "figs"), lang="es", rerun=rerun)
    make_extra_plots(cells, os.path.join(outdir, "figs", "en"), lang="en", rerun=rerun)
    if rerun:
        json.dump(rerun, open(os.path.join(outdir, "replay_fix.json"), "w"), indent=1)
    draw_topologies(os.path.join(outdir, "figs"))
    done = sum(1 for c in cells if c["done"])
    print("celdas analizadas: %d (DONE: %d, FAILED: %d); figuras: %d; salida en %s" % (
        len(cells), done, sum(1 for c in cells if c["failed"]), len(figs), outdir))


if __name__ == "__main__":
    main()
