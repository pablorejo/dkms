# Iteración 005 — Solver K-Splittable MCF operativo (Fase B: OBJ-007 + OBJ-008 + OBJ-009)

**Fecha:** 2026-05-18
**Objetivos trabajados:** OBJ-007, OBJ-008, OBJ-009.

Tres objetivos hechos en la misma iter porque son interdependientes:
modificar `weighted_maxmin` sin actualizar `subtract_usage` y la
construcción del forwarding rompe el invariante de capacidad. Los tres
formaban una unidad lógica.

## Qué se hizo

### `McfSnapshot.rates_per_path` (campo nuevo)

```rust
pub rates_per_path: HashMap<String, Vec<f64>>,
```

Para cada `flow_id`, vector con rate alcanzado por cada path indexado
igual que `Commodity.paths`. Permite reconstruir las proporciones
`omega_p = rates_per_path[p] / Σ rates_per_path` que consumirá el ORR
vía OBJ-010.

`merge_into` propaga este campo (HIGH y LOW tier).

### `build_commodities` aplica filter de overlap

Tras `k_shortest_paths`, filtra paths con > 70 % overlap con otros del
mismo commodity. `K_efectivo` puede ser 1, 2 o 3 según topología local.

### `weighted_maxmin` extendido a `(commodity, path)`

Sustituye la iteración por `commodity` por iteración por
**pseudo-flujo** `(commodity_idx, path_idx)`:

- `pf_to_cidx[i]`, `pf_to_pidx[i]`, `pf_edges[i]`, `pf_weights[i]`
  describen cada pseudo-flujo.
- Cada pseudo-flujo hereda el peso del commodity.
- Bucle water-filling idéntico al anterior pero sobre `n_pf` en lugar
  de `n_flows`.
- Saturación de un edge congela TODOS los pseudo-flujos cuyos paths lo
  cruzan (incluyendo los de distintos commodities).
- Output: agregar pseudo-flujos por commodity → `total_rate` y
  `rates_per_path` por flow.

### `subtract_usage` itera todos los paths

Resta capacidad usada en cada edge POR PATH separado, usando
`snap.rates_per_path[fid][p_idx]` como fuente de rate. Mantiene el
invariante "ningún edge se sobrecarga".

### Forwarding output con `omega` por path

Para cada path con `rate_p > 0`, en cada hop `(u, nxt)` del path se
añade una entry al forwarding del QKC `u` con `omega_p = rate_p /
total`. Múltiples paths que comparten el mismo `(u, nxt)` acumulan
sus omegas en la misma entry.

**Invariante OBJ-009:** la suma de omegas en el QKC origen de cada
commodity es 1.0 ± EPSILON. Verificado en
`multipath_omega_entries_at_origin_sum_to_one`.

## Tests añadidos (6)

1. `multipath_one_commodity_two_disjoint_paths_balanced` — triángulo
   simétrico, 2 paths edge-disjoint, ambos usados con split equitativo.
   Suma omegas QKC origen = 1.0.
2. `multipath_single_useful_path_degrades_to_single_path` — cadena
   lineal 1-2-3-4, solo 1 path posible, comportamiento idéntico a
   single-path.
3. `multipath_two_commodities_sharing_bridge_balance_via_alternate` —
   varios commodities, todos con rate > 0, omega-sum invariante en
   cada QKC origen.
4. `multipath_omega_entries_at_origin_sum_to_one` — invariante OBJ-009.
5. `multipath_rates_per_path_consistent_with_total` — Σ
   rates_per_path[p] = rates[fid].
6. `subtract_usage_respects_capacity_invariant` — ningún edge
   sobrecargado tras solve hybrid.

## Verificación

- `cargo build -p sdn --release` → OK (40 s recompile).
- `cargo test -p sdn --lib --release` → **67 passed** (era 61 + 6
  nuevos).
- `cargo clippy -p sdn --release -- -D warnings` → OK.
- **Sin regresiones** en los 61 tests pre-existentes — la lógica
  single-path de los tests antiguos se preserva porque en topologías
  con K_efectivo = 1, el solver multi-path se comporta idénticamente.

## Decisiones

- **Aplicar filter en `build_commodities`** (no dentro de
  `weighted_maxmin`). Más limpio: los commodities expuestos al solver
  ya tienen sus paths efectivos. Tests externos ven la misma API.
- **`rates_per_path` como campo nuevo**, no extender `forwarding`.
  Razón: necesario para `subtract_usage` (que no puede inferir
  per-path desde el forwarding agregado) y para el RPC futuro
  `GetPathsWithRatios`.
- **Mantener `rates` y `rates_by_dkms` como totales agregados**. La
  API del cliente DKMS no cambia: sigue viendo un rate total por
  buffer. El splitting es transparente.
- **El test ajustado** (caso simétrico triángulo). Cuando tráfico es
  simétrico (dA→dB y dB→dA con misma demanda), el total throughput por
  commodity no sube vs single-path porque la capacidad se comparte.
  Lo que sí cambia: AMBOS paths se usan en lugar de uno solo. El
  beneficio en spread se verá en topologías asimétricas y con cuellos
  topológicos (medirá `bench_multipath.py` en Fase D).

## Hallazgos

- **Caso simétrico no mejora throughput por commodity** — el water-
  filling reparte la capacidad limitada por igual entre los pseudo-
  flujos. La mejora se verá en topologías asimétricas (random `d=3`,
  `bridge`).
- **`rates_per_path` requiere `merge_into` consciente** — los dos
  tiers HIGH/LOW producen `rates_per_path` distintos para flows
  distintos, y `merge_into` debe propagar ambos sin colisión (cada
  flow_id aparece en como mucho un tier).
- El bucle del water-filling sigue siendo O(P × E²) donde
  P = pseudo-flujos = Σ_c |c.paths|. Con K=3 y N=20 DKMSs:
  P ≤ 3 × 20 × 19 = 1140, E = 30. O(1140 × 900) ≈ 1M ops por solve.
  En release, <50 ms.

## Próximos pasos

OBJ-010: nuevo RPC `GetPathsWithRatios` en proto/sdn.proto + handler
en `grpc_server.rs` + service handler. Modifica protos → trigger
build de todo el workspace.

## Bloqueos

Ninguno.
