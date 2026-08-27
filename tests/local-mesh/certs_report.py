#!/usr/bin/env python3
"""Compara las celdas de la campaña de certificados: RSA vs ML-DSA.

Responde a las dos preguntas de la campaña:

  * **¿funcionan los certificados?**  -> handshakes fallidos, rechazos de
    autenticación, respuestas no-200 y corrupción de material.
  * **¿baja el rendimiento?**         -> claves servidas, claves/s, latencia
    p50/p95/p99 y coste del handshake (media y máximo), lado a lado.

El coste del handshake sale de las líneas `tls.stats` del DKMS y no de la
latencia por petición: el cliente mantiene conexiones keep-alive, así que el
handshake se amortiza y no se vería en los percentiles.

    ./certs_report.py ~/dkms_rust/certs-results

Espera el árbol que deja `certs.sbatch`:  <raíz>/<key_alg>/<topo>-n<N>-<reg>/
Es py3.6-safe a propósito: en CESGA corre el python del sistema.
"""
import json
import os
import re
import sys


def pct(sorted_vals, q):
    if not sorted_vals:
        return float("nan")
    i = int(round((len(sorted_vals) - 1) * q))
    return sorted_vals[i]


def read_cell(d):
    """Métricas de una celda. Streaming: los CSV de `mucha` son enormes."""
    cell = {"dir": d, "meta": {}, "status": {}, "keys": 0, "lat": [],
            "hs": 0, "hs_failed": 0, "hs_ms_sum": 0.0, "hs_ms_max": 0.0,
            "auth_reject": 0, "tls_fail": 0, "mldsa_keys": 0, "corrupt": 0,
            "t_min": None, "t_max": None}
    try:
        with open(os.path.join(d, "meta.json")) as fh:
            cell["meta"] = json.load(fh)
    except Exception:
        pass

    for name in sorted(os.listdir(d)):
        if not (name.startswith("load.") and name.endswith(".csv")):
            continue
        with open(os.path.join(d, name)) as fh:
            fh.readline()  # cabecera
            for line in fh:
                p = line.rstrip("\n").split(",")
                if len(p) < 6:
                    continue
                st = p[2]
                # Las filas agregadas de 429/503 traen `xN` en n_keys.
                if p[1] == "-1" and p[5].startswith("x"):
                    try:
                        cell["status"][st] = cell["status"].get(st, 0) + int(p[5][1:])
                    except ValueError:
                        pass
                    continue
                cell["status"][st] = cell["status"].get(st, 0) + 1
                if st == "200":
                    try:
                        cell["keys"] += int(p[4])
                        cell["lat"].append(float(p[3]))
                        t = float(p[0])
                        if cell["t_min"] is None or t < cell["t_min"]:
                            cell["t_min"] = t
                        if cell["t_max"] is None or t > cell["t_max"]:
                            cell["t_max"] = t
                    except ValueError:
                        pass

    re_hs = re.compile(r"handshakes=(\d+).*?failed=(\d+).*?avg_ms=([\d.]+).*?max_ms=([\d.]+)")
    for name in sorted(os.listdir(d)):
        if not (name.startswith("dkms") and name.endswith(".log")):
            continue
        with open(os.path.join(d, name), errors="replace") as fh:
            for line in fh:
                if "tls.stats" in line:
                    m = re_hs.search(line)
                    if m:
                        n = int(m.group(1))
                        cell["hs"] += n
                        cell["hs_failed"] += int(m.group(2))
                        cell["hs_ms_sum"] += float(m.group(3)) * n
                        cell["hs_ms_max"] = max(cell["hs_ms_max"], float(m.group(4)))
                elif "auth.reject" in line:
                    cell["auth_reject"] += 1
                elif "tls handshake failed" in line:
                    cell["tls_fail"] += 1
                elif "ML-DSA-65 cargada" in line:
                    cell["mldsa_keys"] += 1
                elif "recv_corrupt=" in line:
                    m2 = re.search(r"recv_corrupt=(\d+)", line)
                    if m2:
                        cell["corrupt"] = max(cell["corrupt"], int(m2.group(1)))
    return cell


def fmt(cell):
    lat = sorted(cell["lat"])
    dur = (cell["t_max"] - cell["t_min"]) if cell["t_min"] and cell["t_max"] else 0.0
    ok = cell["status"].get("200", 0)
    total = sum(cell["status"].values())
    # 429/503 NO son fallos: son la cuota (token bucket / buffer vacío)
    # haciendo su trabajo bajo saturación. Mezclarlos con los errores reales
    # haría parecer que los certificados fallan cuando lo que pasa es que la
    # carga supera lo que la fibra da de sí.
    throttled = cell["status"].get("429", 0) + cell["status"].get("503", 0)
    errors = total - ok - throttled
    return {
        "alg": cell["meta"].get("key_alg", "?"),
        "reg": cell["meta"].get("regimen", "?"),
        "n": cell["meta"].get("n", "?"),
        "llenos": "%s/%s" % (cell["meta"].get("llenos", "?"), cell["meta"].get("total", "?")),
        "ok": ok,
        "keys_s": (cell["keys"] / dur) if dur > 0 else 0.0,
        "thr_pct": (100.0 * throttled / total) if total else 0.0,
        "err": errors,
        "p50": pct(lat, 0.50), "p95": pct(lat, 0.95), "p99": pct(lat, 0.99),
        "hs": cell["hs"],
        "hs_ms": (cell["hs_ms_sum"] / cell["hs"]) if cell["hs"] else 0.0,
        "hs_max": cell["hs_ms_max"],
        "malas": cell["hs_failed"] + cell["tls_fail"] + cell["auth_reject"],
        "mldsa": cell["mldsa_keys"],
        "corrupt": cell["corrupt"],
    }


def main(argv):
    root = argv[1] if len(argv) > 1 else "certs-results"
    cells = []
    for alg in sorted(os.listdir(root)):
        ad = os.path.join(root, alg)
        if not os.path.isdir(ad):
            continue
        for cd in sorted(os.listdir(ad)):
            d = os.path.join(ad, cd)
            if os.path.isdir(d):
                cells.append(fmt(read_cell(d)))
    if not cells:
        print("sin celdas en %s" % root)
        return 1

    print("== ¿FUNCIONAN LOS CERTIFICADOS? ==")
    hdr = "%-10s %-6s %8s %10s %8s %8s %8s" % (
        "certs", "carga", "hs_ok", "hs+auth_ko", "errs_carga", "corrupt", "claves_ML-DSA")
    print(hdr)
    for c in cells:
        print("%-10s %-6s %8d %10d %8d %8d %8d" % (
            c["alg"], c["reg"], c["hs"], c["malas"], c["err"], c["corrupt"], c["mldsa"]))

    print("\n== ¿BAJA EL RENDIMIENTO? ==")
    print("%-10s %-6s %10s %10s %7s %7s %8s %8s %8s %9s %8s" % (
        "certs", "carga", "200", "claves/s", "%cuota", "errs", "p50_ms", "p95_ms",
        "p99_ms", "handshake", "hs_max"))
    for c in cells:
        print("%-10s %-6s %10d %10.1f %6.1f%% %7d %8.1f %8.1f %8.1f %8.1fms %7.1fms" % (
            c["alg"], c["reg"], c["ok"], c["keys_s"], c["thr_pct"], c["err"],
            c["p50"], c["p95"], c["p99"], c["hs_ms"], c["hs_max"]))

    # Comparación directa por régimen: es la respuesta a "si baja el
    # rendimiento". Se toma rsa como referencia.
    print("\n== ML-DSA vs RSA (mismo régimen) ==")
    by = {}
    for c in cells:
        by.setdefault(c["reg"], {})[c["alg"]] = c
    for reg in sorted(by):
        pair = by[reg]
        base = pair.get("rsa")
        pqc = next((v for k, v in pair.items() if k != "rsa"), None)
        if not base or not pqc:
            print("  %-6s: falta un arm (%s)" % (reg, ", ".join(sorted(pair))))
            continue
        def delta(a, b):
            return ("%+.1f%%" % (100.0 * (b - a) / a)) if a else "n/a"
        print("  %-6s claves/s %.1f -> %.1f (%s) | p95 %.1f -> %.1f ms (%s) | "
              "handshake %.1f -> %.1f ms (%s)" % (
                  reg, base["keys_s"], pqc["keys_s"], delta(base["keys_s"], pqc["keys_s"]),
                  base["p95"], pqc["p95"], delta(base["p95"], pqc["p95"]),
                  base["hs_ms"], pqc["hs_ms"], delta(base["hs_ms"], pqc["hs_ms"])))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
