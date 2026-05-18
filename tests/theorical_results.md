# Modelo teórico de saturación max-min — `dkms-topo`

Este documento describe el **modelo teórico** con el que la CLI
`dkms-topo` compara la rate de saturación observada en cada DKMS. El
objetivo es predecir, para cada par `(src_dkms, dst_dkms)` y bajo un
modelo idealizado de control de admisión max-min, **a qué rate se llena
el buffer ENC del src** mientras todos los demás flows también compiten
por las mismas aristas.

El modelo es una versión **uniforme-peso** del solver del SDN real
(`sdn/src/mcf.rs::weighted_maxmin`, ver
[CLAUDE.md](../CLAUDE.md) sección "Solver SDN — weighted max-min
híbrido"). Con peso 1.0 por commodity el tier-split HIGH/LOW colapsa a
un único `weighted_maxmin` sobre la capacidad completa, así que reuso
ese mismo algoritmo aquí.

## 1. Visión general

### 1.1 Capacidad por arista

Una arista QKD entre dos nodos con `quditto_rate_r0 = R0` y
`distance_km = d` aplica un decay exponencial expresado en dB / 10 km:

```
C_edge = R0 × 10^(-α · d / 10)
```

Con los defaults del orchestator (`R0=2000`, `α=0.2`, `d=5`) el factor
es `10^(-0.1) ≈ 0.7943`, así que `C_edge = 2000 × 0.7943 ≈ 1588.7 kps`.

### 1.2 Bug R-011 — `α=0` ignorado por el orchestator

El orchestator IGNORA el campo `quditto_rate_alpha=0` del payload y
aplica `α=0.2` por defecto (bug pre-existente en `orchestrator/`,
fuera del alcance de esta CLI — ver CLAUDE.md "Tasa teórica
fair-share — NO es R0"). En consecuencia:

- La CLI emite `α=0.0` en el payload (transparente al user).
- El cálculo teórico usa `α=0.2` (lo que realmente aplica el cluster)
  via `DEFAULT_EFFECTIVE_ALPHA = 0.2` en `tests/cli/maxmin_theory.py`.
- Override con `--alpha-override <valor>` en el subcomando para asumir
  otro α (por ejemplo si el bug se arregla aguas arriba).

### 1.3 Commodities

Un **commodity** es un par ordenado `(src_dkms, dst_dkms)` con `src ≠ dst`.
Sobre N nodos hay N(N-1) commodities. Cada commodity sigue un único path
(BFS shortest path con la adyacencia determinista del builder).

### 1.4 Pesos

El modelo teórico usa peso `w_f = 1.0` por commodity. El SDN real usa
pesos decade-spaced (Priority=10000, Important=1000, ...) pero en
condiciones de saturación todos los flows convergen al mismo tier — la
aproximación uniforme es la correcta a estado estacionario.

## 2. Algoritmo max-min (progressive filling)

Mismo algoritmo que `tests/cli/maxmin_theory.py::weighted_maxmin`:

```python
def weighted_maxmin(commodities, capacities, weights=None):
    edge_list = sorted(capacities.keys())
    edge_idx = {ek: i for i, ek in enumerate(edge_list)}
    n_edges = len(edge_list)
    n_flows = len(commodities)

    flow_edges = [[] for _ in range(n_flows)]
    for i, c in enumerate(commodities):
        for ek in path_edges(c.get("path", [])):
            idx = edge_idx.get(ek)
            if idx is not None:
                flow_edges[i].append(idx)

    weights = weights or {}
    flow_w = [float(weights.get(c["id"], 1.0)) for c in commodities]
    caps = [float(capacities[ek]) for ek in edge_list]

    rates = [0.0] * n_flows
    remaining = caps.copy()
    active = [flow_w[i] > 0.0 and bool(flow_edges[i]) for i in range(n_flows)]

    EPS = 1e-9
    guard = 0
    while True:
        guard += 1
        if guard > n_edges + 2:
            break

        edge_w = [0.0] * n_edges
        for i in range(n_flows):
            if not active[i]:
                continue
            w = flow_w[i]
            for e in flow_edges[i]:
                edge_w[e] += w

        delta = math.inf
        for e in range(n_edges):
            if edge_w[e] > EPS and remaining[e] > EPS:
                d = remaining[e] / edge_w[e]
                if d < delta:
                    delta = d
        if not math.isfinite(delta) or delta <= EPS:
            break

        for i in range(n_flows):
            if active[i]:
                rates[i] += flow_w[i] * delta
        for e in range(n_edges):
            if edge_w[e] > EPS:
                remaining[e] -= edge_w[e] * delta
                if remaining[e] < EPS:
                    remaining[e] = 0.0

        saturated = {e for e in range(n_edges) if remaining[e] <= EPS}
        if not saturated:
            break
        for i in range(n_flows):
            if not active[i]:
                continue
            if any(e in saturated for e in flow_edges[i]):
                active[i] = False
        if not any(active):
            break

    return {commodities[i]["id"]: rates[i] for i in range(n_flows) if rates[i] > 0.0}
```

El snippet anterior es **idéntico semánticamente** al de
`sdn/src/mcf.rs::weighted_maxmin` (Rust). Cualquier divergencia entre
observado y predicción que NO sea atribuible a pérdidas de bootstrap,
arranque o asimetría de SDN debe poderse imputar al modelo.

## 3. Fórmulas cerradas por topología

Para topologías regulares, la rate mínima por flow se puede obtener sin
correr el solver, identificando el **edge más cargado** y dividiendo su
capacidad por el número de flows que lo atraviesan.

### 3.1 Ring N

Cada arista soporta el mismo número de flows por simetría. Con N nodos
y N aristas en círculo:

- Distancia promedio entre dos nodos: `avg_path_length`.
- Flows totales = N(N-1) ordered.
- Edge-uses totales = N(N-1) · avg_path_length.
- Flows por edge (uniforme por simetría) = (N-1) · avg_path_length.

**N par**:
```
avg_path_length = 1/(N-1) × [ 2 · Σ_{k=1}^{N/2-1} k  +  N/2 ]
                = 1/(N-1) × [ (N/2)(N/2-1) + N/2 ]
                = 1/(N-1) × N²/4
                = N² / [4(N-1)]
```

**N impar**:
```
avg_path_length = 1/(N-1) × [ 2 · Σ_{k=1}^{(N-1)/2} k ]
                = (N+1)/4
```

**Rate mínima por flow** (BFS perfectamente balanceado):
```
rate_min = C_edge / [(N-1) · avg_path_length]
```

| N  | avg     | flows/edge | rate / C_edge | ejemplo (C=1588.7) |
|---:|--------:|-----------:|--------------:|-------------------:|
| 3  | 1.0     | 2          | 0.5           | 794.3              |
| 4  | 4/3     | 4          | 0.25          | 397.2              |
| 5  | 1.5     | 6          | 0.1667        | 264.8              |
| 6  | 1.8     | 9          | 0.1111        | 176.5              |
| 8  | 32/14≈2.286 | 16     | 0.0625        | 99.3               |

**Importante**: BFS shortest path puede no estar perfectamente
balanceado (en ring N par, los pares antipodales tienen dos shortest
paths y BFS elige uno arbitrario). El observado puede mostrar
asimetrías; usa el solver numérico para el teórico exacto con el path
que BFS realmente toma.

### 3.2 Line N

Aristas etiquetadas `e_1 = (1,2), e_2 = (2,3), ..., e_{N-1} = (N-1, N)`.

El edge `e_i` divide la line en `{1..i}` (i nodos) y `{i+1..N}` (N-i
nodos). Lo cruzan todos los flows entre los dos lados:

```
flows_per_edge(e_i) = 2 · i · (N-i)
```

El **edge más cargado** es el central, `i = ⌊N/2⌋` o `⌈N/2⌉`:
```
max_flows = 2 · ⌊N/2⌋ · ⌈N/2⌉ = ⌊N²/2⌋
```

**Rate mínima** (los flows pasando por el edge central):
```
rate_min = C_edge / ⌊N²/2⌋
```

| N  | max_flows | rate / C_edge | ejemplo |
|---:|----------:|--------------:|--------:|
| 2  | 2         | 0.5           | 794.3   |
| 3  | 4         | 0.25          | 397.2   |
| 4  | 8         | 0.125         | 198.6   |
| 5  | 12        | 0.0833        | 132.4   |
| 6  | 18        | 0.0556        | 88.3    |

Los flows que **no pasan** por el edge central pueden recibir rates más
altas. En line 4, los flows `1↔2` y `3↔4` reciben `C_edge / 4` cada
uno tras el algoritmo de progressive filling (ver pasos detallados en
`agent-dkms-topo-cli/iterations/iteration_006/notes.md` cuando exista).

### 3.3 Mesh n × m

Topología rectangular n filas × m columnas con conexiones a los
4-vecinos (no diagonal). El edge más cargado es uno de los centrales.

Para mesh n×m con BFS shortest path, no hay una fórmula cerrada simple
porque depende del path concreto que BFS elige cuando hay varios
shortest paths de la misma longitud (Manhattan distance). El rate
mínimo cae aproximadamente como:

```
rate_min  ≈  C_edge / [ Θ(n·m·max(n,m)) ]
```

Para mesh 4×4 con 16 nodos: rate_min observado ≈ C_edge / 60 ≈ 26 kps
con C_edge=1588.7 (verificado en `tests/plots/`, sesión 2026-05-17).

**Recomendación**: usar el solver numérico (`maxmin_theory.weighted_maxmin`)
para predecir mesh n×m. No hay valor en formulas cerradas aquí.

### 3.4 Star — 1 centro + B ramas × P nodos cada una

Total nodos: `N = 1 + B·P`. Estructura: cada rama es una line de P
nodos colgando del centro (`c`):

```
c ── b1 ── b2 ── ... ── bP
```

Los edges en una rama, ordenados desde el centro:
- `e_branch[1] = (c, b1)`: lo cruzan los flows entre `{b1..bP}` (P
  nodos) y el resto del sistema (`N-P = 1 + (B-1)P` nodos).
- `e_branch[i] = (b_{i-1}, b_i)`: lo cruzan los flows entre
  `{b_i..bP}` y el resto.

```
flows_per_edge(c, b_1) = 2 · P · (1 + (B-1) · P)
```

Para `B ≥ 2` y `P ≥ 1`, este es el edge más cargado. Por tanto:

```
rate_min = C_edge / [ 2 · P · (1 + (B-1)·P) ]
```

| B | P | rate_min / C_edge | ejemplo |
|--:|--:|------------------:|--------:|
| 2 | 1 | 1/4 = 0.25        | 397.2   |
| 2 | 2 | 1/12 ≈ 0.0833     | 132.4   |
| 3 | 2 | 1/20 = 0.05       | 79.4    |
| 4 | 1 | 1/8 = 0.125       | 198.6   |
| 4 | 2 | 1/28 ≈ 0.0357     | 56.7    |

### 3.5 Random conexo (n, avg_degree, seed)

No hay fórmula cerrada por construcción. Use el solver numérico:

```python
from tests.cli.topology_builders import build_random
from tests.cli.maxmin_theory import (
    topology_to_graph, topology_to_capacities,
    all_pairs_commodities, weighted_maxmin,
)

topo = build_random(n=10, avg_degree=3.0, seed=42)
g = topology_to_graph(topo)
caps = topology_to_capacities(topo, alpha=0.2)
comms = all_pairs_commodities(g)
rates = weighted_maxmin(comms, caps)
```

Para reproducibilidad, fijar `seed`. Con la misma `seed` y mismos
parámetros, la topología y el output del solver son idénticos.

## 4. Cómo se usa esto desde la CLI

1. `dkms-topo <topo> --buffer-saturated` lanza la sim, captura logs y
   produce `sat_analysis.json` con la rate observada por commodity.
2. La CLI calcula la rate teórica con `maxmin_theory.weighted_maxmin(...)`
   sobre la topología y la capacidad ajustada por α (default 0.2 para
   capturar el bug R-011).
3. El criterio de calidad es `t_observed / t_teorico` (medido como
   `(buffer_size / rate_observed) / (buffer_size / rate_teorico) =
   rate_teorico / rate_observed`). El objetivo es **ratio ≥ 0.90**
   para considerar el SDN saludable (Criterio OBJ-025).

## 5. Limitaciones

- **Single-path BFS**: el solver real del SDN permite `k_shortest_paths`
  para repartir un commodity entre varios paths cuando el optimizador
  determina que es mejor. El teórico usa `k=1`. En topologías con
  muchos ciclos cortos (mesh denso, random alto grado), esta
  aproximación puede sub-estimar la rate del sistema.
- **Peso 1.0 uniforme**: el SDN real aplica weights por clase QoS
  (`Priority=10000`, `Important=1000`, etc.) y el solver hybrid
  (HIGH/LOW) los procesa por tiers. En saturación estable todos los
  flows convergen al mismo tier — la aproximación es correcta — pero
  durante transiciones el observado puede divergir.
- **No-reordering, sin pérdidas**: el modelo asume que cada key
  generada en el src llega al dst sin pérdidas y sin reordering.
  Pérdidas reales (timeouts ACK, bootstrap incompleto) bajan la rate
  efectiva en orden de pocos %.
- **Granularidad de commodity = `(src_dkms, dst_dkms)`**: ignora SAEs
  individuales. Esto coincide con el SDN real, donde el reparto
  fair-share es entre flows DKMS, no SAEs (ver
  `memory/project_per_sae_fairness_design.md` para la propuesta de
  cambiar esto a futuro).
- **Capacidad bidireccional implícita**: el modelo trata cada edge como
  una capacidad escalar compartida entre las dos direcciones. El
  hardware QKD real funciona así (la sesión de keys es full-duplex
  sobre un único enlace), pero un análisis más fino requeriría
  capacidades direccionales separadas.
- **Sin overhead de control plane**: el algoritmo no descuenta keys
  consumidas por bootstrap ML-KEM, ACK socket, ni overheads de
  encapsulación. En la práctica el overhead es < 1% pero ahí está.
- **BFS dependiente del orden de adyacencia**: cuando hay varios
  shortest paths, BFS elige uno determinado por el orden de la
  adjacency list del builder. Si el observado usa otro path, los
  rates divergen aunque el teórico sea correcto formalmente.
- **Sin modelo de bootstrap delay**: la primera saturación tras arranque
  toma tiempo extra (~120-300s ORR↔ORR bootstrap ML-KEM, ver CLAUDE.md
  "SDN phantom-Priority starvation"). El teórico predice steady state,
  no warm-up.

## 6. Sanity checks

Para topología `ring 4` con `R0=2000`, `α=0.2`, `d=5`:
- `C_edge = 1588.66 kps`.
- `flows/edge = 4` (uniforme).
- `rate_min teórico = 1588.66 / 4 = 397.2 kps`.

Si el observado da 350 kps mediano, ratio = `397.2 / 350 = 1.135` →
el sistema **está dando MÁS** que el teórico (ratio > 1) o, más
probablemente, hay menos flows compartiendo la edge (algunos no
arrancaron). Si el observado da 320 kps mediano, ratio = `1.24` →
mismo análisis. Si el observado da 397 kps mediano, ratio = `1.000` →
predicción exacta.

`t_observed / t_teorico` se interpreta inverso: con `t = buffer_size / rate`,
ratios > 1 significan saturación más lenta que la prevista; ratios < 1,
saturación más rápida.

## 7. Referencias

- `tests/cli/maxmin_theory.py` — implementación del solver.
- `sdn/src/mcf.rs::weighted_maxmin` — implementación de referencia
  (Rust, producción).
- `CLAUDE.md` secciones "Tasa teórica fair-share — NO es R0" y
  "Solver SDN — weighted max-min híbrido".
- `memory/project_solver_hybrid_tiers.md` — diseño del solver tier
  HIGH/LOW.
- `memory/project_per_sae_fairness_design.md` — propuesta de
  granularidad por-SAE (pendiente).
