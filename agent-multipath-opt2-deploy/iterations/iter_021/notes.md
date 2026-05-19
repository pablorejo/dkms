# iter_021 — OBJ-017 [x] + OBJ-018 [x] + OBJ-019 BLOQUEADO

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.

## OBJ-017 CERRADO [x]

Sim 53 (bridge --cluster-n 4 --cluster-count 2, post-multipath):
- saturation 600s → **24/56 saturated** (igual que baseline)
- median_ratio 1.567 (vs 1.027 baseline = +52%)
- loadtest 2241 requests, **100% OK**.

### Métricas R-017 bridge

| Métrica | Baseline | Post | Delta | R-017 req | Cumple |
|---|---|---|---|---|---|
| M1 SDN CV | 0.874 | 0.933 | **+6.8 %** | ≥-30 % | **❌** |
| M3 SDN sum | 1587 kps | 1589 kps | +0.1 % | no caer >10 % | ✓ |
| M5 starv (rate<1) | 42.9 % | 42.9 % | **0.0 %** | ≥-50 % | **❌** |
| Loadtest éxito | n/a | 2241/2241 = 100 % | — | ≥95 % | ✓ |

**bridge NO cumple R-017**. Algoritmo degenera a comportamiento single-path en topología con cuello único (no hay paths alternativos para flujos cross-cluster).

### Discrepancia snapshot vivo vs final

iter_020 capturó snapshot mid-sim con CV=0.849 y starv=7.1% — interpretación temporal incorrecta. Al final el SDN converge a equilibrio igual a baseline porque el "puente" entre clusters es el único enlace utilizable para flujos cross-cluster.

## OBJ-018 CERRADO [x]

Ejecutado `tests/cli/bench_multipath.py` para las 3 topologías. Outputs en `iter_021/bench/`:
- `metrics_{pequena_densa,mediana_operador,cuello_bridge}.json`
- `compare_{pequena_densa,mediana_operador,cuello_bridge}.png` (gráficas comparativas)
- `bench_results.md` (resumen completo)

### Bench C1-C5 (fill-ratio criterios distintos de R-017)

| Topología | passed_count |
|---|---|
| mesh3x3 | **4/5** ✓ |
| random n=20 d=3 | **0/5** ❌ |
| bridge --c4 --n2 | **1/5** ❌ |

C3 (caída producción ≤10%) revela que **random cae -17.4 %** (total keys 10515408 → 8686000). Esto es más estricto que el M3 manual (rate último), que daba +0.2%. **El total agregado SÍ cae en random**.

## OBJ-019 EVALUACIÓN R-017

| Criterio | mesh | random | bridge | Pasa en | Req | OK |
|---|---|---|---|---|---|---|
| M1 ≥-30 % | ✓ (-70%) | ❌ (+97%) | ❌ (+7%) | 1/3 | ≥2/3 | **❌** |
| M3 no caer >10 % (rate último) | ✓ (+184%) | ✓ (+0.2%) | ✓ (+0.1%) | 3/3 | 3/3 | ✓ |
| M3 (total keys producidas) | ✓ (+4.3%) | ❌ (-17.4%) | ✓ (+0.6%) | 2/3 | 3/3 | **❌** |
| M5 ≥-50 % | ✓ (-86%) | ❌ (-37%) | ❌ (0%) | 1/3 | ≥2/3 | **❌** |

**R-017 NO CUMPLIDO globalmente** — M1 falla en 2/3, M5 falla en 2/3, M3 (total keys) falla en 1/3 (random).

## Decisión

Según `.objetives.md` OBJ-019 + R-016 + R-017:
- Si Fase F detecta <2/3 topologías cumpliendo R-017: **PARAR**, escribir `Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS`.
- NO iterar parámetros automáticamente (R-016).

**Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS.**

Fase G (OBJ-020, OBJ-021 cierre + commit) y Fase H (OBJ-022..026 deploy producción) **NO se ejecutan**.

## Análisis técnico (informativo, NO accionado)

Multipath funciona estructuralmente. El problema es de **calidad del fairness** en topologías sparse/cuello:

1. **Random sparse**: Multipath asigna rates muy heterogéneos (max sube de 100 a 470 kps; otros se quedan a 0). CV explota, fairness empeora.
2. **Bridge cuello**: No hay paths alternativos para flujos cross-cluster → algoritmo degenera a single-path.

R-016 prohíbe ajustar `DEFAULT_OVERLAP_THRESHOLD`, pesos QoS, K, distribución del sampler. Decisión humana requerida para retomar.

## Artefactos OBJ-017 + OBJ-018

- `iter_019/post/cuello_bridge/`: sat_analysis.json, loadtest_analysis.json, 6 CSVs, 8 plots, 8 DKMS logs.
- `iter_021/bench/`: 3 metrics_*.json, 3 compare_*.png, bench_results.md.

## Bloqueos

**Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS** (escrito en `.results.md`).
