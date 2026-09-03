#!/usr/bin/env python3
"""Reduce los CSV de `sae_load` de una fase (L1|L2) a agregados pequeños.
Python >= 3.6 (nodo de cómputo de CESGA). Streaming: a N=100 y bucle cerrado
un CSV por maestro trae millones de filas y no caben en memoria.

    campaign_reduce.py <dir de la celda> <fase>

Escribe <fase>.persec.csv (por segundo: claves OK, peticiones OK, 429, 503,
otros, latencia media), <fase>.pairs.csv (por par ordenado: claves OK, 429,
503) y <fase>.summary.json (totales, percentiles de latencia a partir de un
histograma de 0.1 ms, y la distribución por par).

Formato de entrada (sae_load): t_unix,thread,status,latency_ms,n_keys,key_id,slave.
Con --aggregate-throttled las filas 429/503 vienen agregadas por segundo con
thread=-1 y key_id="x<n>" (n = peticiones que representa).
"""
import csv
import glob
import json
import os
import sys


def main():
    out, tag = sys.argv[1], sys.argv[2]
    files = sorted(glob.glob(os.path.join(out, "%s.sae_*.csv" % tag)))
    persec = {}
    pairs = {}
    hist = [0] * 100001          # 0.1 ms por celda hasta 10 s; la última = overflow
    ok_keys = ok_req = n429 = n503 = n_other = 0
    lat_sum = 0.0
    lat_max = 0.0
    t_min = t_max = None
    for f in files:
        master = os.path.basename(f).split(".")[1]
        with open(f, newline="") as fh:
            rd = csv.reader(fh)
            header = next(rd, None)
            if not header:
                continue
            col = {name: i for i, name in enumerate(header)}
            it, ist, il, ink, ik, isl = (col["t_unix"], col["status"], col["latency_ms"],
                                         col["n_keys"], col["key_id"], col["slave"])
            for r in rd:
                try:
                    t = float(r[it])
                except (ValueError, IndexError):
                    continue
                sec = int(t)
                st = r[ist]
                key = (master, r[isl])
                p = pairs.setdefault(key, [0, 0, 0])
                ps = persec.setdefault(sec, [0, 0, 0, 0, 0, 0.0, 0])
                if t_min is None or t < t_min:
                    t_min = t
                if t_max is None or t > t_max:
                    t_max = t
                if st == "200":
                    nk = int(r[ink] or 1)
                    lat = float(r[il])
                    ok_keys += nk
                    ok_req += 1
                    lat_sum += lat
                    if lat > lat_max:
                        lat_max = lat
                    b = int(lat * 10)
                    hist[b if b < 100000 else 100000] += 1
                    p[0] += nk
                    ps[0] += nk
                    ps[1] += 1
                    ps[5] += lat
                    ps[6] += 1
                else:
                    kid = r[ik] or ""
                    w = int(kid[1:]) if kid.startswith("x") and kid[1:].isdigit() else 1
                    if st == "429":
                        n429 += w
                        p[1] += w
                        ps[2] += w
                    elif st == "503":
                        n503 += w
                        p[2] += w
                        ps[3] += w
                    else:
                        n_other += w
                        ps[4] += w
    with open(os.path.join(out, "%s.persec.csv" % tag), "w") as fh:
        fh.write("t_unix,ok_keys,ok_req,n429,n503,n_other,lat_mean_ms\n")
        for sec in sorted(persec):
            v = persec[sec]
            fh.write("%d,%d,%d,%d,%d,%d,%.3f\n" % (sec, v[0], v[1], v[2], v[3], v[4],
                                                  (v[5] / v[6]) if v[6] else 0.0))
    with open(os.path.join(out, "%s.pairs.csv" % tag), "w") as fh:
        fh.write("master,slave,ok_keys,n429,n503\n")
        for (m, s) in sorted(pairs):
            v = pairs[(m, s)]
            fh.write("%s,%s,%d,%d,%d\n" % (m, s, v[0], v[1], v[2]))

    def pct(q):
        if ok_req == 0:
            return 0.0
        target = q * ok_req
        acc = 0
        for i, c in enumerate(hist):
            acc += c
            if acc >= target:
                return i / 10.0
        return lat_max

    per_pair = sorted(v[0] for v in pairs.values())
    summary = {
        "phase": tag, "files": len(files),
        "t_first": t_min, "t_last": t_max,
        "span_s": (t_max - t_min) if (t_min is not None and t_max is not None) else 0.0,
        "ok_keys": ok_keys, "ok_req": ok_req, "n429": n429, "n503": n503, "n_other": n_other,
        "lat_mean_ms": (lat_sum / ok_req) if ok_req else 0.0,
        "lat_p50_ms": pct(0.50), "lat_p90_ms": pct(0.90), "lat_p99_ms": pct(0.99), "lat_max_ms": lat_max,
        "pairs_seen": len(pairs), "pairs_zero": sum(1 for v in per_pair if v == 0),
        "pair_keys_min": per_pair[0] if per_pair else 0,
        "pair_keys_median": per_pair[len(per_pair) // 2] if per_pair else 0,
        "pair_keys_max": per_pair[-1] if per_pair else 0,
    }
    json.dump(summary, open(os.path.join(out, "%s.summary.json" % tag), "w"), indent=1, sort_keys=True)
    print(json.dumps(summary))


if __name__ == "__main__":
    main()
