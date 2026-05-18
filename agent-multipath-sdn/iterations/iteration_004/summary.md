# Iteración 004 — `filter_overlapping_paths` (Fase B: OBJ-006)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-006.

## Qué se hizo

`sdn/src/mcf.rs`: nueva función `filter_overlapping_paths(paths, threshold) -> Vec<Path>` + constante pública `DEFAULT_OVERLAP_THRESHOLD = 0.70`.

Algoritmo:
- Itera paths en orden (Yen's los devuelve ascendente por longitud).
- Cada nuevo `p` se compara con cada `k` ya aceptado: `|edges(p) ∩ edges(k)| / |edges(p)| > threshold` → descartar.
- Resultado: `K_efectivo ∈ {1, …, K}`.

**Casos edge cubiertos:**
- Path degenerado (0 edges, src==dst) → descartado.
- Input vacío → output vacío.
- Threshold configurable; tests con 0.50, 0.70, 0.80 demuestran sensibilidad correcta.

**Razonamiento del 70 %:** heurístico. En topos de operador con grado medio bajo (2–4), descartar paths que comparten > 70 % de edges es agresivo pero no demasiado. Documento `project_multipath_design.md` §5.4 explica que el filtro NO es un requisito de corrección, sino una optimización (menos pseudo-flujos, mejor sampling, solve más rápido).

## Tests añadidos (7)

1. `filter_keeps_disjoint_paths` — 2 paths edge-disjoint → ambos mantenidos.
2. `filter_drops_near_identical_path` — boundary: 2/3 (66.7 %) NO descarta, 3/4 (75 %) sí.
3. `filter_threshold_configurable` — mismo input, threshold 0.50 vs 0.80 → diferente resultado.
4. `filter_three_paths_one_redundant` — 3 paths donde p3 == p1 → mantiene 2.
5. `filter_drops_zero_edge_path` — path degenerado (1 nodo) → descartado.
6. `filter_default_threshold_constant_is_0_70` — verifica const pública.
7. `filter_empty_input_returns_empty` — `[]` → `[]`.

## Verificación

- `cargo build -p sdn --release` → OK (1m 01s).
- `cargo test -p sdn --lib --release` → **61 passed** (era 54 + 7 nuevos).
- `cargo clippy -p sdn --release -- -D warnings` → OK.

## Decisiones

- **Función PÚBLICA** (`pub fn`): para que la pueda llamar `weighted_maxmin` en OBJ-007 y para testabilidad externa.
- **Threshold via parámetro, no const fija**: permite que el solver use `DEFAULT_OVERLAP_THRESHOLD` pero tests y futuras configuraciones puedan tunearlo.
- **Reusa `path_edges`** ya existente (`mcf.rs:229`): zero código duplicado.
- **Conservación de orden:** se respeta el orden de input (Yen's devuelve shortest-first), no se reordenan por longitud ni nada.

## Hallazgos

- `path_edges()` retornaba `Vec<EdgeKey>`; `HashSet::from_iter` directo no compila por lifetimes → uso `.into_iter().collect()`.
- El bucle es O(K²) en el número de paths, K∈{1,2,3} típicamente → trivial. Para K más grande habría que usar índice de edges-vistos.

## Próximos pasos

**OBJ-007:** extender `weighted_maxmin` para iterar sobre `(commodity, path_idx)`. Es el cambio sustancial de la Fase B. Tests propuestos: 1 commodity 2 paths sin cuello (split 50/50), 1 commodity 2 paths con cuello (split favorece libre), 2 commodities compartiendo paths.

## Bloqueos

Ninguno.
