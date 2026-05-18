# Iteración 012 — Bench mesh 3x3 baseline (Fase D: OBJ-014 2/3)

**Fecha:** 2026-05-18

## Estado de sims

- **Sim 49 mesh 3x3 (pequena_densa) — COMPLETO** ✓
  - Sat: 42/72 (58.3 %), mediana ratio 0.949.
  - SAE: 2265 reqs, 100 % OK.
  - CSVs: `iter_009/baseline/pequena_densa_mesh3x3/data/` (6 archivos).
  - Stop tuvo el WARN típico de `Read timed out` (documentado).

- **Sim 50 bridge (cuello_bridge) — EN CURSO**
  - Sat: 8/56 a ~3 min (lento, esperado por el cuello).
  - PID 2313867 activo.
  - Esperable: 15-20 min total.

## Métricas baseline registradas

| Métrica | Mesh 3x3 | Mediana operador |
|---|---:|---:|
| M1 spread_fill_ratio | 0.347 | 0.762 |
| M2 min_fill_ratio | **0.616** ✓ | 0.202 |
| M3 total_production_keys | 3,853,613 | 10,515,408 |
| M4 saturation_time_seconds | 10,485 | 12,505 |
| M5 max_starvation_continuous_sec | 215 | 425 |
| median_fill_ratio | 0.951 | 0.337 |
| % saturados | 58.3 (42/72) | 12.6 (48/380) |

## Análisis

- **Mesh 3x3 ya cumple M2** (0.616 ≥ 0.40). M1 (0.347) cerca de cumplir (target ≤ 0.25). Solo M5 (starvation 215s) claramente alto.
- **Mediana operador NO cumple ninguno** — es el caso típico que multi-path debe arreglar.
- **Hipótesis confirmada:** topologías densas (mesh) ya están cerca del óptimo single-path. Multi-path aportará principalmente en cuellos topológicos (bridge será el test crítico).

## Pendiente

- Iter 013 (≈23:13): verificar bridge sim 50 terminada → bench + completar el set de 3 baselines → marcar OBJ-014 [x].
