# bench_results.md — Fase F resumen completo

**Fecha:** 2026-05-19
**Generado por:** iter_021 (OBJ-018).

## Resumen ejecutivo

Multipath K-Splittable MCF **funciona estructuralmente** (no panics, no errors, loadtest 100% en 3 topologías). Pero **NO cumple R-017 globalmente**.

- **mesh 3x3** (densa, 9 nodos): el caso de uso ideal. M1 -70 %, M3 +184 %, M5 -86 %. **R-017 PASA con margen enorme.**
- **random n=20 d=3** (sparse): M1 empeora (+97 %), M3 cae -17.4 % (>10 %), M5 mejora sólo -37.5 %. **R-017 FALLA.**
- **bridge --c4 --n2** (cuello): M1 +6.8 %, M3 +0.1 %, M5 0.0 %. **R-017 FALLA (algoritmo se comporta como single-path).**

## Tabla R-017 (M1/M3/M5 del agent prompt, calculadas desde sat_analysis.json)

| Topología | M1 CV base→post | Δ% | M3 sum base→post (kps) | Δ% | M5 starv% base→post | Δ% |
|---|---|---|---|---|---|---|
| mesh3x3 | 1.192 → 0.357 | **-70.0 %** ✓ | 2382 → 6774 | **+184 %** ✓ | 58.3 → 8.3 | **-85.7 %** ✓ |
| random n=20 d=3 | 0.603 → 1.187 | **+96.7 %** ❌ | 12917 → 12940 | +0.2 % ✓ (último rate) | 12.6 → 7.9 | -37.5 % ❌ |
| bridge --c4 --n2 | 0.874 → 0.933 | **+6.8 %** ❌ | 1587 → 1589 | +0.1 % ✓ | 42.9 → 42.9 | 0.0 % ❌ |

| Criterio | Pasa en topologías | Req | Decisión |
|---|---|---|---|
| M1 ≥-30 % | mesh (1/3) | ≥2/3 | **❌** |
| M3 no caer >10 % | mesh, random (último rate), bridge (3/3 con rate último; 2/3 con total keys C3) | 3/3 | **mixed** |
| M5 ≥-50 % | mesh (1/3) | ≥2/3 | **❌** |

## Tabla bench_multipath C1-C5 (criterios fill-ratio)

| Topología | C1 spread ≤0.25 | C2 min_fill ≥0.4 | C3 prod caída ≤10% | C4 t-sat caída ≥60% | C5 starv ≤60s | passed_count |
|---|---|---|---|---|---|---|
| mesh3x3 | 0.177 ✓ | 0.777 ✓ | -4.32% ✓ | 94.99% ✓ | 195s ❌ | **4/5** |
| random n=20 d=3 | 0.795 ❌ | 0.166 ❌ | **17.4%** ❌ | 45.30% ❌ | 425s ❌ | **0/5** |
| bridge --c4 --n2 | 0.619 ❌ | 0.349 ❌ | 0.60% ✓ | 25.71% ❌ | 290s ❌ | **1/5** |

## Causa probable del fallo

### Por qué multipath funciona en mesh3x3 pero no en random/bridge

**mesh3x3**: Cada par de nodos tiene K=2-3 paths cortos y de longitud similar. Multipath reparte el rate ~equitativamente entre paths. Edges no-cuello se saturan en paralelo → throughput agregado se multiplica.

**random n=20 d=3**: Topología sparse aleatoria. Algunos pares tienen 1 path (bridge), otros tienen 3-4. Multipath asigna rates **muy heterogéneamente** según paths disponibles → max rate sube de 100 kps (baseline) a 470 kps (post) en commodities ricos, mientras otros se quedan a 0. CV explota +97 %.

**bridge --c4 --n2**: 2 clusters de 4 conectados por 1 enlace. Multipath para flujos cross-cluster TODOS pasan por ese único enlace → no hay paths alternativos reales. El algoritmo termina degenerando a single-path.

### Hipótesis algoritmica (NO ACCIONADA por R-016)

- El K-Splittable MCF con `DEFAULT_OVERLAP_THRESHOLD = 0.70` puede ser demasiado permisivo en topologías sparse: paths con 70 % de edges compartidos se cuentan como caminos distintos pero compiten por la misma capacidad. **NO toco este parámetro** (R-016).
- Los pesos QoS pueden estar masivamente sesgados hacia clases superiores en commodities ricos. **NO toco**.
- El número K=3 de paths puede ser excesivo para topologías sparse. **NO toco**.

## Loadtest SAE (operacional, no parte de R-017 pero relevante)

| Topología | Requests | OK% | Throttled% | Errors% | p95 latency |
|---|---|---|---|---|---|
| mesh3x3 | 2262 | 100.0 | 0.0 | 0.0 | 49.8 ms |
| random n=20 d=3 | 2240 | 100.0 | 0.0 | 0.0 | 84.3 ms |
| bridge --c4 --n2 | 2241 | 100.0 | 0.0 | 0.0 | (no medido aquí) |

**El path E2E SAE→DKMS→ORR→QKC→...→QKC→ORR→DKMS→SAE funciona sin errores en las 3 topologías**. El bloqueo R-017 NO es operacional sino de calidad fairness/throughput.

## Imágenes Docker desplegadas

- `pablopio/sdn:v8` 138 MB digest sha256:0c95e4a6dfc7…
- `pablopio/orr:v8.1` ~137 MB digest sha256:539356300becfd…
- `pablopio/qkc:v8` 136 MB digest sha256:6eb465c440f2…

`ORR_MULTIPATH_ENABLED=true` activo en orchestator deploy.

## Gráficas comparativas

- `compare_pequena_densa.png` — mesh3x3 baseline vs post (multipath claro)
- `compare_mediana_operador.png` — random n=20 d=3 baseline vs post (deterioro)
- `compare_cuello_bridge.png` — bridge baseline vs post (~similar)

## Decisión OBJ-019

Aplicando R-017 + R-016:

- M1 cumple en **1/3** topologías (mesh). Requiere ≥2/3 → **NO**.
- M5 cumple en **1/3** topologías (mesh). Requiere ≥2/3 → **NO**.
- M3 según interpretación: por rate último 3/3 pero por total keys (C3) 2/3 (random cae -17.4 %).

**Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS.**

Fase G (OBJ-020, OBJ-021) y Fase H (OBJ-022-026) **NO se ejecutan**. R-016 prohíbe iterar parámetros automáticamente. Decisión humana requerida.

### Acciones recomendadas para retomar (manual, fuera de este agente)

1. Revisar `DEFAULT_OVERLAP_THRESHOLD = 0.70` en `sdn/src/mcf.rs`: bajar a 0.50 o 0.40 para topologías sparse.
2. Considerar K=2 (menos paths) en bridge/sparse.
3. Revisar pesos QoS en commodities con `max_rate >> mean_rate`.
4. Considerar añadir un cap por commodity en el solver MCF.

**El rollback path está intacto** (R-010): `kubectl -n dkms-main-ns set env deploy/orchestator ORR_MULTIPATH_ENABLED- SDN_IMAGE=pablopio/sdn:v7 ORR_IMAGE=pablopio/orr:v7 QKC_IMAGE=pablopio/qkc:v7` revierte a comportamiento single-path.
