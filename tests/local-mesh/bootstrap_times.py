#!/usr/bin/env python3
"""Cuánto tarda cada módulo en quedar operativo, desde que arranca su proceso.

No instrumenta nada: se apoya en las líneas periódicas que ya emite cada
módulo (`qkc.links`, `orr.state`, `generator.state`) y en `starts.tsv`, que
`mesh.sh` escribe con el instante exacto en que lanza cada proceso.

El t0 es el lanzamiento y no el primer log del módulo, a propósito: entre uno
y otro está la inicialización, que es justamente parte de lo que se quiere
medir.

Resolución: las líneas de estado salen **cada 5 s**, así que un hito que se
detecta por ellas tiene esa granularidad. Los hitos que salen de un evento
puntual (el anuncio, el primer enlace) son exactos. Se marca cuál es cuál con
`±5s` en la salida para no vender precisión que no hay.

    ./bootstrap_times.py [directorio-de-la-malla]
"""
from __future__ import annotations

import os
import re
import sys
from datetime import datetime, timezone

ANSI = re.compile(r"\x1b\[[0-9;]*m")
# 2026-08-20T14:19:06.399817Z al principio de cada línea de tracing.
TS = re.compile(r"^(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z")


def stamp(line: str) -> float | None:
    m = TS.match(line)
    if not m:
        return None
    return datetime.fromisoformat(m.group(1)).replace(tzinfo=timezone.utc).timestamp()


def scan(path: str, predicate) -> float | None:
    """Instante de la primera línea que cumple `predicate`, o None."""
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                line = ANSI.sub("", raw)
                if predicate(line):
                    return stamp(line)
    except FileNotFoundError:
        return None
    return None


def num(line: str, field: str) -> int | None:
    m = re.search(rf"\b{field}=(\d+)", line)
    return int(m.group(1)) if m else None


def starts(logs: str) -> dict[str, float]:
    out: dict[str, float] = {}
    try:
        with open(os.path.join(logs, "starts.tsv")) as fh:
            for line in fh:
                mod, _, t = line.strip().partition("\t")
                if mod and t:
                    out[mod] = float(t)
    except FileNotFoundError:
        pass
    return out


# ─── hitos por rol ─────────────────────────────────────────────────────────
#
# (etiqueta, predicado, ¿lo detecta una línea periódica?)  El tercer campo es
# lo que decide si el número lleva el ±5s.

QKC = [
    ("anunciado a la SDN", lambda l: "anunciado a la SDN" in l, False),
    # `waiting=[]` con `live` no vacío: todos los vecinos declarados montados.
    # Un QKC sin vecinos nunca lo cumple, y es correcto que no aparezca.
    ("todos los enlaces montados",
     lambda l: "qkc.links" in l and "waiting=[]" in l and "live=[]" not in l, True),
    ("primer material de clave",
     lambda l: "keystore.levels" in l and (num(l, "enc") or 0) > 0, True),
]

ORR = [
    ("anuncio aceptado",
     lambda l: "anunciado a la SDN" in l and "accepted=true" in l, False),
    # Bootstrap PQC cerrado con TODOS los pares: hasta aquí, lo que se le mande
    # a los que falten es indescifrable.
    ("bootstrap con todos los pares",
     lambda l: ("orr.state" in l and (num(l, "peers") or 0) > 0
                and num(l, "with_master") == num(l, "peers")), True),
]

DKMS = [
    ("anuncio aceptado",
     lambda l: "anunciado a la SDN" in l and "accepted=true" in l, False),
    ("peers recibidos de la SDN", lambda l: "peers actualizados por la SDN" in l, False),
    ("primera clave recibida",
     lambda l: "generator.state" in l and (num(l, "recv") or 0) > 0, True),
]

ROLES = {"qkc": QKC, "orr": ORR, "dkms": DKMS}


def buffers_full(path: str, capacity: int = 4096) -> float | None:
    """Cuándo el ENC de **todos** los peers llegó a capacidad.

    Se mira por peer y se coge el más tardío: el módulo no está del todo listo
    mientras le falte uno, aunque el resto lleven rato llenos.
    """
    first_full: dict[str, float] = {}
    peers: set[str] = set()
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            for raw in fh:
                line = ANSI.sub("", raw)
                if "generator.state" not in line:
                    continue
                m = re.search(r"peer=(\S+)", line)
                if not m:
                    continue
                peer = m.group(1)
                peers.add(peer)
                if peer not in first_full and (num(line, "enc") or 0) >= capacity:
                    t = stamp(line)
                    if t:
                        first_full[peer] = t
    except FileNotFoundError:
        return None
    if not peers or len(first_full) < len(peers):
        return None
    return max(first_full.values())


def main() -> int:
    root = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        os.path.dirname(os.path.abspath(__file__)), "..", "results", "local-mesh")
    logs = os.path.join(root, "logs")
    if not os.path.isdir(logs):
        print(f"no hay logs en {logs}", file=sys.stderr)
        return 2

    t0 = starts(logs)
    if not t0:
        print("falta starts.tsv: la malla se levantó con un mesh.sh anterior",
              file=sys.stderr)
        return 2

    mesh_t0 = min(t0.values())
    rows: list[tuple[str, str, float, bool]] = []
    for mod, start in sorted(t0.items()):
        role = re.sub(r"\d+$", "", mod)
        if role not in ROLES:
            continue
        path = os.path.join(logs, f"{mod}.log")
        for label, pred, periodic in ROLES[role]:
            t = scan(path, pred)
            if t is not None:
                rows.append((mod, label, t - start, periodic))
        if role == "dkms":
            t = buffers_full(path)
            if t is not None:
                rows.append((mod, "buffers llenos con todos", t - start, True))

    if not rows:
        print("ningún hito alcanzado todavía", file=sys.stderr)
        return 1

    # Resumen por hito: lo que importa es el peor caso, que es cuando la malla
    # entera está lista, no el módulo más rápido.
    by_label: dict[tuple[str, str], list[float]] = {}
    for mod, label, dt, periodic in rows:
        role = re.sub(r"\d+$", "", mod)
        by_label.setdefault((role, label), []).append(dt)

    print(f"  malla levantada en {logs}")
    print(f"  {len(t0) - 1} módulos + SDN\n")
    print(f"  {'módulo':<6} {'hito':<28} {'mín':>7} {'mediana':>8} {'máx':>7}  n")
    print("  " + "-" * 66)
    order = {"qkc": 0, "orr": 1, "dkms": 2}
    periodic_of = {(re.sub(r"\d+$", "", m), l): p for m, l, _, p in rows}
    for (role, label), vals in sorted(by_label.items(),
                                      key=lambda kv: (order.get(kv[0][0], 9), kv[0][1])):
        vals.sort()
        med = vals[len(vals) // 2]
        mark = " ±5s" if periodic_of.get((role, label)) else ""
        print(f"  {role:<6} {label:<28} {vals[0]:6.1f}s {med:7.1f}s "
              f"{vals[-1]:6.1f}s  {len(vals)}{mark}")

    last = max(dt + t0[mod] for mod, _, dt, _ in rows)
    print(f"\n  malla operativa entera: {last - mesh_t0:.1f}s desde el primer arranque")
    print("  (±5s = detectado por una línea de estado periódica, que sale cada 5 s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
