#!/usr/bin/env python3
"""Generador de topologías de la campaña 2026-09 (Python >= 3.6: corre en el
nodo de cómputo de CESGA, que tiene el 3.6 del sistema).

    topologies.py <familia> <N> [--seed 42] [--r0 2000] [--alpha 0.2]
                  [--dist-km 5] [--rgg-radius-km 30] [--rgg-degree 4]
                  [--edges | --meta | --both]

Familias (id de nodo 1..N; cada arista lleva SU distancia en km):

  estrella   hub 1 + N-1 hojas.
  anillo     ciclo puro C_N. Edge-transitivo a todo N, que es lo que quiere un
             barrido en N (el `ring` de mesh.sh añade cuerdas cuyo número
             depende de N mod 3 y no es comparable entre tamaños).
  puente     dos ciclos C_{N/2} unidos por UNA arista (1, N/2+1): el corte
             mínimo es 1, todo el tráfico entre comunidades pasa por ahí.
  malla      rejilla rectangular r×c de 4 vecinos con r·c = N, lo más
             cuadrada posible (10=2×5, 20=4×5, 30=5×6, …, 100=10×10).
  rgg        grafo geométrico aleatorio, el mismo modelo del paper
             (`tests/cli/topology_builders.py::build_rgg` de la rama
             tests_cesga): N puntos uniformes en [0,L]², L = r·sqrt(π·N/k),
             arista si d ≤ r, y conexidad forzada con un árbol de expansión
             aleatorio si hace falta. Cada arista lleva su distancia
             euclídea, luego su capacidad QKD R0·10^(-α·d/10) es distinta.
  aleatoria  el `random` de mesh.sh, replicado con la misma semilla: árbol de
             expansión aleatorio + aristas extra hasta max(N-1, 3N/2).

Salida:
  --edges  "a-b:d a-b:d …" para DKMS_MESH_EDGES (mesh.sh, topología custom)
  --meta   JSON con aristas, grados, saltos medios (pares ordenados),
           diámetro, capacidad por arista y los dos techos:
             techo_fibra   = Σ cap_e / saltos_medios  (cota superior: cada
                             clave gasta una clave QKD por salto)
             techo_cuello  = min_e cap_e·P / carga_e, con carga_e el número de
                             caminos más cortos (par ordenado, reparto igual
                             entre caminos empatados) que cruzan e. Es el
                             techo del enlace más cargado con demanda uniforme.
  --both   las dos cosas: el JSON lleva además "edges_env".
"""
import json
import math
import random
import sys
from collections import deque


def cap_keys_per_s(r0, alpha, d_km):
    return r0 * 10.0 ** (-alpha * d_km / 10.0)


# ── familias ──────────────────────────────────────────────────────────────
def fam_estrella(n, d):
    return {(1, k): d for k in range(2, n + 1)}


def fam_anillo(n, d):
    es = {}
    for k in range(1, n + 1):
        a, b = k, k % n + 1
        es[(min(a, b), max(a, b))] = d
    return es


def fam_puente(n, d):
    if n % 2:
        raise SystemExit("puente: N debe ser par")
    h = n // 2

    def cycle(base, m):
        out = {}
        for k in range(1, m + 1):
            a, b = base + k, base + (k % m) + 1
            out[(min(a, b), max(a, b))] = d
        return out

    es = cycle(0, h)
    es.update(cycle(h, h))
    es[(1, h + 1)] = d
    return es


def fam_malla(n, d):
    best = None
    for r in range(1, int(math.sqrt(n)) + 1):
        if n % r == 0:
            best = r
    r, c = best, n // best
    if r < 2:
        raise SystemExit("malla: N=%d no admite rejilla de al menos 2 filas" % n)
    es = {}

    def nid(i, j):
        return 1 + i * c + j

    for i in range(r):
        for j in range(c):
            if j + 1 < c:
                es[(nid(i, j), nid(i, j + 1))] = d
            if i + 1 < r:
                es[(nid(i, j), nid(i + 1, j))] = d
    return es


def fam_rgg(n, radius_km, avg_degree, seed):
    rng = random.Random(seed)
    side = radius_km * math.sqrt(math.pi * n / avg_degree)
    pts = [(rng.random() * side, rng.random() * side) for _ in range(n)]
    es = {}

    def dist(i, j):
        dx = pts[i][0] - pts[j][0]
        dy = pts[i][1] - pts[j][1]
        return math.sqrt(dx * dx + dy * dy)

    for i in range(n):
        for j in range(i + 1, n):
            dd = dist(i, j)
            if dd <= radius_km:
                es[(i + 1, j + 1)] = dd
    # Conexidad: el builder del paper añadía un árbol de expansión ALEATORIO
    # entero, que a N=100 (L≈266 km) metía ~60 aristas de 100-250 km con
    # capacidad ~0 — enlaces muertos que no son "QKD a distancia RGG" sino
    # ruido. Aquí se une cada componente por su par cruzado MÁS CERCANO
    # (bosque de expansión mínimo, Kruskal sobre los pares no adyacentes):
    # el mínimo de aristas extra, con la mínima distancia posible.
    if not connected(n, es):
        parent = list(range(n))

        def find(x):
            while parent[x] != x:
                parent[x] = parent[parent[x]]
                x = parent[x]
            return x

        for (a, b) in es:
            ra, rb = find(a - 1), find(b - 1)
            if ra != rb:
                parent[ra] = rb
        cands = sorted(((dist(i, j), i, j) for i in range(n) for j in range(i + 1, n)
                        if (i + 1, j + 1) not in es), key=lambda t: t[0])
        for dd, i, j in cands:
            ri, rj = find(i), find(j)
            if ri != rj:
                parent[ri] = rj
                es[(i + 1, j + 1)] = dd
                if connected(n, es):
                    break
    return es, pts


def fam_aleatoria(n, d, seed):
    rnd = random.Random(seed)
    edges = set()
    order = list(range(1, n + 1))
    rnd.shuffle(order)
    for i in range(1, len(order)):
        a, b = order[i], order[rnd.randrange(i)]
        edges.add((min(a, b), max(a, b)))
    target = max(n - 1, (3 * n) // 2)
    todos = [(a, b) for a in range(1, n + 1) for b in range(a + 1, n + 1)]
    rnd.shuffle(todos)
    for e in todos:
        if len(edges) >= target:
            break
        edges.add(e)
    return {e: d for e in edges}


# ── métricas ──────────────────────────────────────────────────────────────
def adjacency(n, es):
    adj = {k: [] for k in range(1, n + 1)}
    for a, b in es:
        adj[a].append(b)
        adj[b].append(a)
    return adj


def connected(n, es):
    adj = adjacency(n, es)
    seen = {1}
    q = deque([1])
    while q:
        u = q.popleft()
        for v in adj[u]:
            if v not in seen:
                seen.add(v)
                q.append(v)
    return len(seen) == n


def metrics(n, es, r0, alpha):
    """Saltos medios, diámetro y carga por arista con caminos más cortos
    (Brandes: sigma y reparto igual entre caminos empatados)."""
    adj = adjacency(n, es)
    load = {e: 0.0 for e in es}
    hops_sum = 0
    diam = 0
    for s in range(1, n + 1):
        dist = {s: 0}
        sigma = {s: 1.0}
        order = []
        q = deque([s])
        while q:
            u = q.popleft()
            order.append(u)
            for v in adj[u]:
                if v not in dist:
                    dist[v] = dist[u] + 1
                    sigma[v] = 0.0
                    q.append(v)
                if dist[v] == dist[u] + 1:
                    sigma[v] += sigma[u]
        if len(dist) != n:
            raise SystemExit("grafo no conexo")
        # delta[v] = fracción de caminos (s→cualquiera) que pasan por v
        delta = {v: 0.0 for v in dist}
        for w in reversed(order):
            for v in adj[w]:
                if dist[v] == dist[w] - 1:
                    c = sigma[v] / sigma[w] * (1.0 + delta[w])
                    delta[v] += c
                    e = (min(v, w), max(v, w))
                    load[e] += c
        for v, h in dist.items():
            hops_sum += h
            diam = max(diam, h)
    pairs = n * (n - 1)
    mean_hops = hops_sum / pairs
    caps = {e: cap_keys_per_s(r0, alpha, d) for e, d in es.items()}
    sum_cap = sum(caps.values())
    techo_fibra = sum_cap / mean_hops
    techo_cuello = min(caps[e] * pairs / load[e] for e in es if load[e] > 0)
    e_cuello = min(es, key=lambda e: caps[e] * pairs / load[e] if load[e] > 0 else float("inf"))
    degs = [len(adj[k]) for k in adj]
    return {
        "pairs": pairs,
        "mean_hops": round(mean_hops, 4),
        "diameter": diam,
        "degree_mean": round(sum(degs) / n, 3),
        "degree_min": min(degs),
        "degree_max": max(degs),
        "sum_cap_keys_per_s": round(sum_cap, 1),
        "cap_min": round(min(caps.values()), 1),
        "cap_max": round(max(caps.values()), 1),
        "techo_fibra_keys_per_s": round(techo_fibra, 1),
        "techo_cuello_keys_per_s": round(techo_cuello, 1),
        "arista_cuello": "%d-%d" % e_cuello,
        "carga_max_par_paths": round(max(load.values()), 2),
    }


def main(argv):
    if len(argv) < 3:
        raise SystemExit(__doc__)
    fam, n = argv[1], int(argv[2])
    opts = {"--seed": "42", "--r0": "2000", "--alpha": "0.2", "--dist-km": "5",
            "--rgg-radius-km": "30", "--rgg-degree": "4"}
    mode = "--both"
    i = 3
    while i < len(argv):
        a = argv[i]
        if a in ("--edges", "--meta", "--both"):
            mode = a
            i += 1
        elif a in opts:
            opts[a] = argv[i + 1]
            i += 2
        else:
            raise SystemExit("opción desconocida: %s" % a)
    seed = int(opts["--seed"])
    r0, alpha, d = float(opts["--r0"]), float(opts["--alpha"]), float(opts["--dist-km"])
    pts = None
    if fam == "estrella":
        es = fam_estrella(n, d)
    elif fam == "anillo":
        es = fam_anillo(n, d)
    elif fam == "puente":
        es = fam_puente(n, d)
    elif fam == "malla":
        es = fam_malla(n, d)
    elif fam == "rgg":
        es, pts = fam_rgg(n, float(opts["--rgg-radius-km"]), float(opts["--rgg-degree"]), seed)
    elif fam == "aleatoria":
        es = fam_aleatoria(n, d, seed)
    else:
        raise SystemExit("familia desconocida: %s (estrella|anillo|puente|malla|rgg|aleatoria)" % fam)
    if not connected(n, es):
        raise SystemExit("la topología %s N=%d no es conexa" % (fam, n))
    edges_env = " ".join("%d-%d:%.4g" % (a, b, dd) for (a, b), dd in sorted(es.items()))
    if mode == "--edges":
        print(edges_env)
        return
    meta = {"family": fam, "n": n, "seed": seed, "r0": r0, "alpha": alpha,
            "edges": len(es), "link_type": "qkd"}
    meta.update(metrics(n, es, r0, alpha))
    meta["edge_list"] = [[a, b, round(dd, 4)] for (a, b), dd in sorted(es.items())]
    if pts is not None:
        meta["rgg"] = {"radius_km": float(opts["--rgg-radius-km"]),
                       "avg_degree_target": float(opts["--rgg-degree"]),
                       "side_km": round(float(opts["--rgg-radius-km"]) * math.sqrt(
                           math.pi * n / float(opts["--rgg-degree"])), 3),
                       "points": [[round(x, 3), round(y, 3)] for x, y in pts]}
    if mode == "--both":
        meta["edges_env"] = edges_env
    print(json.dumps(meta, sort_keys=True))


if __name__ == "__main__":
    main(sys.argv)
