# iter_027 — wait-state OBJ-027 (sim 55 random a 5m52s, snapshot positivo M5)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-027 (sim 55 random sigue corriendo).

## Estado actual

```
PID 3024929 ELAPSED=05:52 STAT=Sl
sim_id=55 ns=55 Active 5m43s
4/380 commodities saturated (bootstrap completo, curva inicial)
saturation_timeout 600s → restante ~4 min
```

## Snapshot rate distribution (5 DKMS de 20, 95 samples)

| DKMS    | peers | min | median | max | sum | CV   |
|---------|-------|-----|--------|-----|-----|------|
| dkms-878 | 19   | 6   | 48     | 84  | 831 | 0.63 |
| dkms-881 | 19   | 6   | 54     | 191 | 996 | 0.95 |
| dkms-884 | 19   | 5   | 45     | 76  | 718 | 0.66 |
| dkms-887 | 19   | 6   | 32     | 79  | 728 | 0.74 |
| dkms-890 | 19   | 0   | 28     | 55  | 414 | 0.82 |

**Agregado (95 samples, 5/20 DKMS)**:
- min=0.0, median=30.6 kps, max=**191.4** kps (vs sim52 final 469.8)
- sum=3688 kps (5/20 DKMS)
- **CV preliminar = 0.840**
- **starvation (<1 kps) = 1/95 = 1.1 %**

## Comparativa preliminar con sim 52 (OVERLAP=0.70)

| Métrica | Baseline single | Sim 52 (0.70) final | Sim 55 (0.50) @5m52s |
|---|---|---|---|
| CV (M1) | 0.603 | 1.187 ❌ R-017 (+97%) | **0.840** (+39% prelim, todavía falla -30%) |
| starvation | 12.6% | 7.9% ❌ R-017 (-37%) | **1.1%** prelim (-91% si se mantiene → CUMPLE -50%!) |
| max rate | 99.9 kps | 469.8 kps | **191.4 kps** preliminar (mucho menos heterogéneo) |

## Análisis del trade-off threshold 0.70 vs 0.50

**mesh densa** (sim 51 vs 54):
- 0.70: R-017 cumple plenamente (M1 -70%, M5 -86%, M3 +184%).
- 0.50: R-017 NO cumple (M1 -29%, M5 -40.5%).
- Conclusión: threshold MÁS permisivo es mejor en densa.

**random sparse** (sim 52 vs 55 preliminar):
- 0.70: R-017 falla M1 (+97%) y M5 (-37%).
- 0.50 preliminar: M5 mejora drásticamente (-91% si se mantiene), M1 mejora pero sigue fallando (+39%).
- Conclusión: threshold MÁS estricto es mejor en sparse para starvation.

**Trade-off es REAL**: ningún threshold único optimiza ambas topologías. R-017 con sólo `DEFAULT_OVERLAP_THRESHOLD` puede no ser alcanzable.

## Decisión iter_027

- **NO interrumpir** sim 55. Esperar números finales.
- Próximo cron (~00:50Z) coge sim 55 cerca de terminar (saturate + loadtest).
- Si random termina:
  - M5 ≤ -50 %: contribuye a 2/3 en M5 (con mesh3x3 -86 % de sim 51, no de sim 54).

  **Ojo**: para comparar fair, el bench R-017 debe usar **TODOS los runs post** (sim 54 mesh + sim 55 random + sim ? bridge) contra **TODOS los baselines (009, 010, 011)**. NO mezclar sim 51 con 54.

## Plan post

Si random sim 55 cumple M5 (-50%) pero falla M1:
- Verificar bridge sim 56 con 0.50.
- Si bridge también mejora M5 → 2/3 en M5. Pero M1 sigue fallando 0-1/3 → R-017 global falla.
- **OBJ-027 con OVERLAP=0.50 cumple PARCIALMENTE pero no R-017 completo**.

Pasar a OBJ-028 con otro parámetro:
- **K_PATHS_PER_FLOW=2**: menos paths → posiblemente mejor M1 en sparse (menos competencia interna).

## Bloqueos

Ninguno. Espera natural.
