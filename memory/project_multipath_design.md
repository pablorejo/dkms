# K-Splittable MCF — diseño de migración del solver SDN

> Estado del documento: **diseño**. Aún no implementado en código (excepto
> el pre-cálculo de K paths con Yen's, que ya existe). El agente
> `agent-multipath-sdn/` ejecuta la implementación incremental.

## 1. Problema

El SDN del DKMS calcula el ratemax-min weighted para cada commodity
`(src_dkms, dst_dkms)` sobre la topología de QKCs. El solver actual
(`sdn/src/mcf.rs`) **pre-calcula K=3 paths** por commodity mediante
Yen's algorithm, pero **solo usa el primero** — descarta los otros dos.

El resultado: en grafos con grado medio `D > 1`, cada commodity solo
aprovecha 1 de los `D` enlaces de salida disponibles. Los otros `D−1`
quedan infrautilizados. Cuando varios commodities multi-hop convergen
en un mismo edge, ese edge se vuelve un **cuello topológico
artificial** que el SDN no puede aliviar con prioridades (todos los
flujos en la misma clase reciben la misma rate; subir la prioridad
de uno no añade capacidad al edge).

**Síntoma medido en `random n=20 d=3`, baseline
`tests/results/20260518T160920Z-random-n20-d3.0-srnd/`:**

- 380 commodities; 48 saturados a los 600 s (12.6 %).
- 80+ commodities < 20 kps, atascados por compartir el mismo cuello
  con otros 4-5 commodities.
- Mediana ratio observed/theoretical = 0.585.
- Spread de fill ratios entre commodities ≈ 0.9 (algunos al 95 %,
  otros al 5 %).

**Solución:** migrar a **K-Splittable MCF** (sinónimo operacional:
**WCMP** — Weighted-Cost Multi-Path). Cada commodity se reparte entre
hasta `K` paths pre-calculados; el water-filling decide
automáticamente las proporciones `omega_p`.

## 2. Fundamento teórico

El solver actual implementa **weighted max-min fairness** vía
water-filling greedy. Para esta función objetivo, el water-filling
produce la asignación lex-max-min única **(Bertsekas-Gallager 1992)**
— es el óptimo exacto, **no una aproximación**.

Importante: el cambio NO es "water-filling → LP". Es "single-path →
multi-path manteniendo water-filling como solver". La generalización
canónica es iterar sobre `(commodity, path)` en lugar de sobre
`commodity` solo.

> "Water-filling resuelve weighted max-min fairness exactamente. El
> LP sólo aportaría mejora si cambiásemos el objetivo a
> max-throughput, lo cual sería **inconsistente** con la semántica de
> QoS classes ('el flow más castigado dentro de cada clase debe estar
> lo más alto posible')."

## 3. Estado actual del código

| Componente | Estado | Comentario |
|---|---|---|
| `k_shortest_paths` (Yen sobre BFS) | ✅ Implementado | `sdn/src/mcf.rs:117`. K=3. |
| `Commodity.paths: Vec<Path>` | ✅ Implementado | Estructura lista para K paths. |
| `McfSnapshot.forwarding` con `omega` | ✅ Existe | `(next_hop, omega)`. Comentario: "*kept multi-path-ready*". Hoy `omega = 1.0` siempre. |
| `Frame.header_qkc_mp` (cabecera QKC) | ✅ Reservado, vacío | "*Reservado para metadatos del propio QKC; hoy va vacío.*" — disponible si hace falta. |
| ORR source-routing por hops | ✅ Funciona | `app_header["orr_path"]` lleva el path completo. `GetOrrPath(src, dst) → Vec<orr_id>` ya existe. |
| Routing intermedio en QKC | `dest_id → next_hop` (destination-based) | `qkc/src/routing.rs:35`. **NO** per-flow, **NO** per-path. **NO se toca.** |
| Solver multi-path real | ❌ NO | Solo usa `paths.first()` en líneas **375, 441, 537** de `mcf.rs`. |
| API SDN para K paths con ratios | ❌ NO | Solo `get_orr_path → 1 path`. |

**Sorpresa importante:** el mecanismo de source-routing **ya está
implementado y funcionando** en el ORR. El frame ya tiene los campos
necesarios. **No hay que inventar wire format.** El cambio es local a
solver SDN + cliente ORR + un RPC nuevo.

## 4. Decisión sub-prioridades — **pesos status quo**

Antes de codificar había que decidir si las sub-prioridades dentro de
un mismo tier (HIGH = Priority + Important + Quickly; LOW = Relax +
BestEffort) deben ser:

- **(a)** Pesos decade-spaced (10000/1000/100/10/1) en una sola fase
  de water-filling por tier — **status quo**.
- **(b)** N fases estrictas dentro de cada tier (Priority excluye
  Important, Important excluye Quickly, etc.).

**Decisión: mantener pesos status quo (opción a).**

Razonamiento:

1. **La separación HIGH/LOW ya da prioridad estricta entre tiers** —
   un BestEffort no quita capacidad a un Quickly. Eso ya cumple la
   semántica de QoS dura.
2. **Dentro de un tier, queremos suavidad.** Un buffer pasando de
   Priority (fill < 10 %) a Important (fill < 30 %) no debería caer
   bruscamente a rate cero; los pesos 10000/1000 le dan ~10× menos
   capacidad cuando comparte con un flow Priority, **pero no lo
   matan**.
3. **N fases estrictas oscilarían más.** Un buffer cruzando 10 % iría
   de "todo" a "casi nada" (el flow Important obtendría todo el
   sobrante de Priority). Con la histéresis de las clases (95 %/85 %
   entry/exit y 5 % entre niveles intermedios), el flapping sería
   peor.
4. **Coste de implementación.** Mantener una sola fase por tier con
   pesos es **gratis** (lo que hay hoy). N fases por tier requeriría
   refactorizar el bucle de `solve()`.

Este punto se cierra: el cambio multipath **NO toca** la lógica de
sub-prioridades. Solo cambia la unidad de iteración del water-filling
de `commodity` a `(commodity, path)`.

## 5. Algoritmo K-Splittable MCF

### 5.1 Estructura general

Mantiene la arquitectura híbrida HIGH/LOW del solver actual:

```text
solve(commodities, capacities, weights):
    snap = empty snapshot
    remaining = capacities clonado
    high = commodities con weight ≥ TIER_THRESHOLD
    low  = commodities con 0 < weight < TIER_THRESHOLD

    if high not empty:
        sub = weighted_maxmin_multipath(high, remaining, weights)
        subtract_usage_multipath(remaining, high, sub)
        snap.merge(sub)

    if low not empty AND remaining has cap > 0:
        sub = weighted_maxmin_multipath(low, remaining, weights)
        snap.merge(sub)

    return snap
```

### 5.2 Water-filling sobre `(commodity, path)`

```text
weighted_maxmin_multipath(commodities, capacities, weights):
    # Filtrar paths con >70% overlap respecto a otros del mismo commodity.
    for c in commodities:
        c.paths_effective = filter_overlapping_paths(c.paths)

    # Pseudo-flujos: uno por cada (commodity, path_idx).
    pseudo_flows = []
    for c in commodities:
        for path_idx in 0..len(c.paths_effective):
            pseudo_flows.push((c, path_idx, weight=weights[c]))

    rates = {pseudo_flow → 0.0}
    active = {pseudo_flow → true}
    remaining = capacities clonado

    repeat:
        # Peso agregado por edge.
        edge_w = sum_{f active, e in f.path} weight(f)

        # Δ máximo admisible en cada edge: min remaining/edge_w.
        delta = min_{e: edge_w[e] > EPS} remaining[e] / edge_w[e]
        if delta < EPS: break

        # Subir todos los activos por weight × delta.
        for f active: rates[f] += weight(f) × delta

        # Restar capacidad consumida.
        for e: remaining[e] -= edge_w[e] × delta

        # Congelar pseudo-flujos cuyos paths cruzan un edge ahora saturado.
        saturated_edges = {e: remaining[e] ≈ 0}
        for f active:
            if any e in saturated_edges, e in f.path:
                active[f] = false

        if no f active: break

    # Construir output: por commodity, omega_p = rates[(c,p)] / sum_p rates[(c,p)]
    snap.rates[c] = sum_p rates[(c,p)]
    snap.forwarding[c.src_qkc][c.flow_id] = [
        (path_p.next_hop, omega_p) for p in c.paths_effective if rates[(c,p)] > 0
    ]
    return snap
```

Propiedades:

- **Reparto óptimo intra-commodity.** Cuando un path satura, los otros
  del mismo commodity siguen creciendo. El reparto entre los K paths
  de cada commodity **emerge automáticamente** — no es una decisión
  separada.
- **El solver penaliza overlap naturalmente.** Si dos paths del mismo
  commodity comparten edge `e`, ambos consumen capacidad de `e` y
  ambos se congelan cuando `e` satura. El filtro pre-solve (sección
  5.4) es **una optimización**, no un requisito de corrección.
- **Coste:** `O(F·E)` por iteración × como máximo `E` iteraciones =
  `O(F·E²)`. Para la sim mediana (380 commodities × ~30 edges × K=3 →
  ~1140 pseudo-flujos), un water-filling pure-Rust corre en <50 ms.

### 5.3 Sampling en el ORR origen (alias method)

El SDN publica por cada commodity una lista
`[(path_1, omega_1), (path_2, omega_2), …]` con `Σ omega_p ≈ 1.0`.

El ORR del DKMS origen cachea esa lista por destino. Al enviar cada
key:

```text
sample_path(entries):
    # Alias method (Walker, 1977). Construcción O(K), sampling O(1).
    r = uniform(0, 1)
    idx = floor(r × K)
    if uniform(0, 1) < prob[idx]:
        return entries[idx].path
    else:
        return entries[alias[idx]].path
```

El path elegido va en `app_header["orr_path"]` (mecanismo ya existente
y verificado). El QKC sigue siendo destination-based; el path solo lo
conoce el ORR origen y la cebolla.

### 5.4 Filtro de overlap (optimización, no requisito)

Tras Yen's K=3 paths, descartar aquellos con >70 % edges compartidos
respecto a otros del mismo commodity:

```text
filter_overlapping_paths(paths):
    kept = []
    for p in paths (en orden ascendente de longitud):
        for k in kept:
            shared = |edges(p) ∩ edges(k)| / |edges(p)|
            if shared > 0.70:
                skip p
        kept.push(p)
    return kept
```

Resultado: `K_efectivo(commodity) ∈ {1, 2, 3}` dependiendo de la
topología local. En `random d=3`, esperable mayoría a `K=2`.

**Por qué es optimización y no requisito:** el water-filling congela
correctamente paths con edges compartidos (cuando el edge satura,
ambos paths se congelan). El filtro solo:

- Reduce el número de variables (menos pseudo-flujos por commodity).
- Mejora el sampling (evita que el ORR distribuya tráfico entre
  paths casi idénticos, lo cual sería un sample wasted).
- Acelera el solve.

Sin filtro, el solver da resultados correctos pero más lentos y los
ratios tienen entries casi-cero que no aportan.

## 6. Ejemplo numérico paso a paso (3 nodos triángulo)

### Topología

```text
        cap=10
   A ────────── B
    \         /
 cap=10     cap=10
      \    /
        C
```

3 nodos A, B, C. 3 edges no dirigidos, cada uno cap = 10 keys/s
(simplificado para el ejemplo; los caps reales serían
`R0 × 10^(-α·d/10)` ≈ 1588 con defaults).

### Caso 1 — 1 commodity, multi-path AYUDA

- Commodity `c1 = (A → C)`.
- K=2 paths edge-disjoint:
  - `p1 = [A, C]` (1 edge).
  - `p2 = [A, B, C]` (2 edges).
- Peso `w_c1 = 1` (uniforme).

**Single-path (status quo):**

- Solo usa `p1 = [A, C]`.
- Edge AC: rate = w_c1 = 1, cap = 10 → rate × Δ = 10 cuando Δ=10.
- AC satura primero, `r_c1 = 10`.
- Edges AB y BC **quedan vacíos** (desperdicio).
- **Total = 10 keys/s** para c1.

**Multi-path (cambio propuesto):**

Iteración 1:
- Pseudo-flujos activos: `f_{c1,p1}` (en edge AC), `f_{c1,p2}` (en
  edges AB y BC).
- Peso por edge: AB=1, BC=1, AC=1.
- Δ por edge: AB = 10/1 = 10, BC = 10/1 = 10, AC = 10/1 = 10.
- Δ_min = 10.
- Subir: `f_{c1,p1} += 1·10 = 10`, `f_{c1,p2} += 1·10 = 10`.
- Capacidad restante: AB=0, BC=0, AC=0. **Los 3 edges saturan a la vez.**
- Congelar todos los pseudo-flujos.

- `r_{c1,p1} = 10`, `r_{c1,p2} = 10`.
- `r_c1 = 20`. `omega_p1 = 10/20 = 0.5`, `omega_p2 = 10/20 = 0.5`.
- **Total = 20 keys/s — el DOBLE.**

**Lección:** multi-path duplica el throughput cuando existen paths
edge-disjoint (no comparten cuellos).

### Caso 2 — 2 commodities, multi-path NO ayuda al throughput pero balancea carga

- Commodities `c1 = (A→C)`, `c2 = (B→C)`.
- `c1.paths = [AC, ABC]`; `c2.paths = [BC, BAC]`.

**Single-path:**

- `c1` usa AC: cap=10, r_c1 = 10.
- `c2` usa BC: cap=10, r_c2 = 10.
- Edge AB **libre** (desperdiciado).
- **Total = 20.**

**Multi-path:**

Iteración 1:
- Pseudo-flujos: `f_{c1,p1}` (AC), `f_{c1,p2}` (AB+BC), `f_{c2,p1}`
  (BC), `f_{c2,p2}` (BA+AC).
- Peso por edge:
  - AB: f_{c1,p2} + f_{c2,p2} → peso=2.
  - BC: f_{c1,p2} + f_{c2,p1} → peso=2.
  - AC: f_{c1,p1} + f_{c2,p2} → peso=2.
- Δ por edge: AB = 10/2 = 5, BC = 5, AC = 5.
- Δ_min = 5.
- Subir todos a 5.
- Los 3 edges saturan a la vez.
- Congelar todo.

- `r_c1 = 5 + 5 = 10`. `omega = (0.5, 0.5)`.
- `r_c2 = 5 + 5 = 10`. `omega = (0.5, 0.5)`.
- **Total = 20** (mismo que single-path).

**Lección:** en este caso el throughput agregado no mejora, **pero
ahora cada edge se usa exactamente al 100 %** (carga distribuida) en
lugar de tener 1 edge desperdiciado. Esto es relevante para la
métrica de "spread del fill ratio" (criterio principal) y para
robustez ante fallos de edges.

### Caso 3 — 2 commodities con cuello compartido, multi-path AYUDA

Para evitar el caso trivial donde single-path ya estaba óptimo,
considerar topología con cuello compartido. La extensión natural
(`bridge` builder, OBJ-004) crea dos clusters mesh conectados por un
único edge. En ese escenario:

- N commodities pasando por el bridge → single-path los pisa todos
  en un cuello.
- Multi-path no puede romper el min-cut del bridge, pero **balancea
  dentro de cada cluster** los flows que no necesitan cruzar, dando
  más capacidad al bridge.

Beneficio cuantitativo esperado: el commodity más castigado pasa de
~22 kps a ~50-80 kps. Saturados de 48/380 a 80-120/380.

## 7. Cambios concretos en `mcf.rs`

| Sitio | Hoy | Cambio |
|---|---|---|
| **mcf.rs:375** (`subtract_usage`) | `let Some(p) = c.paths.first() else { continue };` | Iterar `for (p_idx, p) in c.paths.iter().enumerate()` y restar `omega_p × r` de cada edge en `p`. |
| **mcf.rs:441** (`weighted_maxmin` build flow_edges) | `let Some(p) = c.paths.first() else { continue };` | Construir `flow_edges` por `(commodity_idx, path_idx)`. Los activos pasan a ser pseudo-flujos. |
| **mcf.rs:537** (output forwarding entries) | `if let Some(p) = c.paths.first() { … omega = 1.0 }` | Por cada path con `rate > 0`, emitir entry con `(next_hop, omega_p = rate_p / Σ rate)`. |

Además:

- **Nueva función** `filter_overlapping_paths(paths, threshold=0.70) -> Vec<Path>`.
- **Tests unitarios** que verifiquen:
  - 1 commodity 2 paths edge-disjoint → split 50/50 (caso 1 arriba).
  - 1 commodity con un path cuello → ratio sesgado hacia el path libre.
  - Filter de overlap: 3 paths con uno casi-idéntico → 2 paths en output.
  - Invariante `Σ omega_p ≈ 1.0` por commodity (tolerancia EPSILON).

## 8. Nuevo RPC `GetPathsWithRatios`

`proto/sdn.proto`:

```protobuf
service Sdn {
  // … existentes …
  rpc GetPathsWithRatios(GetPathsWithRatiosRequest) returns (GetPathsWithRatiosResponse);
}

message GetPathsWithRatiosRequest {
  string src_dkms = 1;
  string dst_dkms = 2;
}

message PathWithRatio {
  repeated string qkc_hops = 1;  // path = secuencia de qkc_ids
  double omega = 2;              // ratio, 0 < omega ≤ 1
}

message GetPathsWithRatiosResponse {
  repeated PathWithRatio paths = 1;
  // Suma de omega ≈ 1.0 (±EPSILON). Si rate=0 para el commodity,
  // se devuelve vacío.
}
```

`sdn/src/grpc_server.rs` y `sdn/src/service.rs`: nueva handler que
consulta el último `McfSnapshot` y proyecta las entries del
`forwarding` para ese `(src,dst)`.

**Compatibilidad:** el RPC `GetForwarding` existente sigue
funcionando (devuelve el primer path para clientes que no migren).

## 9. Coste estimado

| Cambio | Líneas | Sesiones |
|---|---|---|
| `filter_overlapping_paths` + tests | ~80 | 0.3 |
| `weighted_maxmin` extendido + tests | ~150 | 0.5 |
| `subtract_usage` extendido + tests | ~50 | 0.2 |
| Construcción forwarding output + tests | ~50 | 0.2 |
| `GetPathsWithRatios` RPC (proto, server, service) | ~100 | 0.3 |
| ORR sampling alias + cache + tests | ~150 | 0.5 |
| ORR cache invalidation | ~50 | 0.2 |
| Tests CLI (`bridge` builder, `bench_multipath`) | ~200 | 0.5 |
| **Total** | **~830** | **~2.7** |

Sin dependencias externas. Sin cambios en QKC, wire format, ni DKMS.

## 10. Beneficio esperado

Predicciones cuantitativas en `random n=20 d=3` (380 commodities,
600 s):

| Métrica | Baseline | Objetivo |
|---|---:|---:|
| Mediana ratio observed/theoretical | 0.585 | ≥ 0.85 |
| % commodities saturados | 12.6 (48/380) | ≥ 30 % |
| Spread fill ratio | ~0.90 | ≤ 0.25 |
| Fill ratio mínimo a los 600 s | ~0.10 | ≥ 0.40 |
| Throughput agregado | (baseline) | ±10 % (no destruir) |

En topología `bridge` (cuello obvio), el ratio del más castigado debe
pasar de ~22 kps a ≥ 50-80 kps.

## 11. Trade-offs reconocidos

- **Sacrificamos max-throughput agregado por max-min fairness.** En
  casos como Ejemplo 2 (sec 6), single-path con suerte topológica da
  el mismo throughput que multi-path. Multi-path SIEMPRE iguala o
  reduce el spread, pero no siempre añade throughput. **Decisión
  consciente:** la métrica reina es el spread, no el throughput.
- **Migrar a LP en el futuro cambiaría la semántica observable**, no
  solo el solver. Si en algún momento se quiere optimizar
  throughput-agregado o min-max-congestion, será un cambio
  arquitectónico distinto.
- **El sampling introduce varianza.** Con rates < 10 kps el ratio
  empírico puede oscilar visiblemente. Mitigable con alias method de
  buena calidad y aceptar que en régimen sub-Hz el reparto es
  ruidoso. En >100 kps, LLN converge rápido (< 100 envíos).
- **`K_efectivo` por commodity es variable** (1, 2 o 3). En
  topologías con grado mínimo bajo, multi-path simplemente no
  aplica (`K_efectivo = 1`). El solver se comporta exactamente como
  hoy en esos casos.

## 12. Criterios de aceptación (6, deben cumplirse simultáneamente)

Antes de mergear, el cambio debe pasar en ≥ 2 de las 3 topologías de
robustez (`bridge`, `random n=20 d=3`, `mesh 3x3` o equivalente
densa) durante 600 s descartando 30 s iniciales de transitorio:

| # | Criterio | Umbral |
|---|---|---|
| 1 | Spread del fill ratio en régimen estacionario | **≤ 0.25** (vs ~0.9 actual) |
| 2 | Fill ratio mínimo a los 600 s | **≥ 0.40** (vs ~0.10) |
| 3 | Producción total de claves vs baseline | **no caer > 10 %** |
| 4 | Tiempo agregado en saturación (segundos-buffer al 100 %) | **caer ≥ 60 %** |
| 5 | Starvation: tiempo continuo con fill < 0.15 por DKMS | **≤ 60 s** continuos |
| 6 | Solver < 200 ms en topología mediana, memoria añadida < 50 MB, binario añadido < 2 MB | |

Definiciones operacionales (resueltas con el usuario, sesión
2026-05-18):

- **Spread y mínimo:** se miden por commodity (la peor LÍNEA de la
  gráfica `sat_enc_over_time`, no por DKMS).
- **Starvation:** "el DKMS está bajo 0.15" = **alguno** de sus
  buffers está bajo 0.15 (la definición más estricta).
- **Producción total:** `Σ emit_total` de todos los DKMS al final del
  run (métrica que ya está en logs `generator.state`).
- **Topología "cuello obvio":** `bridge` builder nuevo (OBJ-004) que
  crea dos clusters mesh conectados por un único edge.

## 13. Validación pre-implementación

Antes de tocar `mcf.rs`, hay que tener:

- [x] **Decisión sub-prioridades** tomada (sec 4, pesos status quo).
- [x] **Ejemplo numérico de 3 nodos** trabajado (sec 6) que ilustra
      el caso 1 (multi-path duplica throughput) y caso 2 (balancea
      carga sin coste).
- [ ] **`bench_multipath.py`** corriendo sobre el baseline para fijar
      números de partida (OBJ-005, próxima iteración).
- [ ] **Builder `bridge`** implementado (OBJ-004).

Una vez verificado eso, se procede a implementar Fase B con commits
atómicos por sub-cambio.

## 14. Referencias

- Bertsekas & Gallager, *Data Networks*, 2nd ed (1992), cap. 6 — fair
  allocation y water-filling.
- Yen, J., *Finding the k Shortest Loopless Paths*, Management Science
  (1971).
- Koch & Spenke, *Complexity and Approximability of k-Splittable
  Flow*, MFCS (2003) — el nombre "k-splittable".
- Walker, A. J., *An Efficient Method for Generating Discrete Random
  Variables*, ACM TOMS (1977) — alias method.
- Microsoft Azure / Google B4 traffic engineering — uso operacional
  del término "WCMP".

---

**Última actualización:** 2026-05-18 (iter 001 — primer release del
documento). Sucesivas iteraciones del agente `multipath-sdn` podrán
añadir secciones de "Estado de implementación" sin tocar las
secciones 1-14 (que son el contrato del diseño).
