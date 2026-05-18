# Iteración 010 — Baseline mediana operador copiada (Fase D: OBJ-014 EN CURSO 1/3)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-014 (parcial).

## Qué se hizo

1. **Verificada sim 49 (mesh 3x3) aún corriendo** — PID 2297056 sigue activo, progreso lento (10/72 saturados a ~5 min). Esperable.
2. **Copiado baseline mediana operador** desde `tests/results/20260518T160920Z-random-n20-d3.0-srnd/` a `agent-multipath-sdn/iterations/iteration_010/baseline/mediana_operador/`. Incluye `data/{generator_state,per_commodity,theory_rates,loadtest_metrics,loadtest_requests,loadtest_sae_timeline}.csv` + `sat_analysis.json` + `loadtest_analysis.json`.
3. **Verificado bench sobre baseline mediana** — `bench_multipath.py` produce las 5 métricas:

| Métrica | Valor |
|---|---|
| M1 spread_fill_ratio | 0.7619 |
| M2 min_fill_ratio | 0.2023 |
| M3 total_production_keys | 10,515,408 |
| M4 saturation_time_seconds | 12,505 |
| M5 max_starvation_continuous_sec | 425 |

## Pendiente

- **Iter 011 (≈22:57)**: verificar sim 49 mesh 3x3 terminada → copiar sus CSVs a `baseline/pequena_densa/`. Lanzar sim bridge con `--cluster-n 4 --cluster-count 2`, offset 54.
- **Iter 012 (≈23:05)**: verificar bridge terminado → copiar CSVs. Marcar OBJ-014 [x] cuando los 3 baselines estén en disco.

## Bloqueos

Ninguno. EKS responde, BD offset libre, sim corre.
