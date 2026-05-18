# Iteración 002 — Builder `bridge` (Fase A: OBJ-004)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-004.

## Qué se hizo

1. **`tests/cli/topology_builders.py`:** añadida función `build_bridge(cluster_n, cluster_count=2, intra_degree=None)`. Genera N clusters, cada uno un ring de `cluster_n` nodos, conectados por un único edge "bridge" entre cluster i y cluster i+1 (cluster_count-1 bridges totales). Layout: clusters distribuidos horizontalmente. Bridge cruza el "diámetro" del ring (left_port = node 0 del cluster, right_port = node cluster_n//2). `intra_degree` reservado para futura densificación (hoy ignorado, cada cluster es ring puro).

2. **`tests/cli/dkms_topo.py`:**
   - Import añadido a `build_bridge`.
   - Auto-name: `bridge-c{cluster_count}-n{cluster_n}`.
   - Despacho en `_build_topology`.
   - Subparser `bridge` con flags `--cluster-n` (required, >=3) y `--cluster-count` (default 2, >=2).

3. **`tests/cli/test_topology_builders.py`:** 5 tests nuevos:
   - `test_bridge_2_clusters_of_4_counts`: 2 rings de 4 → 8 nodos, 9 edges (4+4+1).
   - `test_bridge_3_clusters_of_5_counts`: 3×5 → 15 nodos, 17 edges (5×3 + 2 bridges).
   - `test_bridge_has_single_min_cut_edge_between_clusters`: propiedad clave — los edges entre clusters son exactamente `cluster_count - 1` (un único cut entre cada par consecutivo).
   - `test_bridge_validation_errors`: rechaza `cluster_n < 3`, `cluster_count < 2`, `intra_degree < 2.0`.
   - `test_bridge_uids_unique_and_sequential`: uids únicos, node_ids 1..N consecutivos.

## Verificación

- `python3 -m pytest tests/cli/test_topology_builders.py -q` → **26 passed** (era 21 + 5 nuevos).
- `python3 -m pytest tests/cli/ -q` → **185 passed** (era 180 + 5 nuevos).
- Smoke CLI: `python3 -m tests.cli.dkms_topo bridge --cluster-n 4 --cluster-count 2 --dry-run` → JSON correcto, 8 nodos, 9 links, name `bridge-c2-n4`, edge bridge entre `node-3 ↔ node-5` (cluster_n // 2 = 2 → uid node-3, primer nodo del cluster 2 = node-5).
- Sin cambios Rust en esta iter, no aplica `cargo build/test/clippy`.

## Decisiones

- **Cluster = ring puro (degree 2).** El nombre y la firma admiten `intra_degree` para futuras variantes (mesh interno, random densificado), pero `None` mantiene el caso patológico: cada cluster con grado 2, así que dentro del cluster ya hay 2 paths posibles (ida/vuelta del anillo) y multi-path puede balancear esa carga sin tocar el bridge.
- **Bridge cruza por el "diámetro".** Conecta `cluster_uids[0]` con `cluster_uids[cluster_n // 2]` (no nodos adyacentes). Esto hace que cualquier tráfico inter-cluster recorra ≥ floor(cluster_n / 4) hops dentro del cluster destino para llegar al bridge port — visualmente claro y operacionalmente realista.
- **`intra_degree` validado pero no usado** todavía. Si el futuro pide densificar clusters, basta extender el builder sin cambiar firma ni subcommand.

## Hallazgos

- Confirmado que `185 passed` antes/después del cambio (180 → 185, los 5 nuevos del bridge). Suite total 7.37 s con matplotlib lazy import; <0.1 s sin él.
- `_assert_fields` exigía `link_type ∈ {"QKD", "PQC", "HYBRID"}` — el default `"QKD"` del builder pasa OK.
- El subcomando bridge respeta el patrón `_add_global_flags(p_bridge)`, lo cual da automáticamente `--name`, `--owner`, `--r0`, `--alpha`, `--distance`, etc.

## Próximos pasos

- **OBJ-005:** `tests/cli/bench_multipath.py` que lee `per_commodity.csv` + `generator_state.csv` y emite las 6 métricas de aceptación. Es la última de Fase A. Después arranca Fase B con OBJ-006 (`filter_overlapping_paths` en `mcf.rs`).

## Bloqueos

Ninguno.
