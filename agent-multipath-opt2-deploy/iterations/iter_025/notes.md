# iter_025 — OBJ-027 cierre parcial mesh3x3 (degrada) + lanzar random n=20 d=3

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo:** seguir OBJ-027 (auto-tune iter 1: OVERLAP=0.50).

## Mesh 3x3 CERRADA — DEGRADA vs OVERLAP=0.70

Sim 54 terminó:
- saturated: **25/72** (vs 6/72 con 0.70; vs 42/72 baseline single)
- median_ratio: 1.212
- loadtest: **2168 requests, 100% OK** ✓
- Stop limpio (Read timed out cliente bug conocido; ns 54 NotFound).

### Comparativa mesh 3x3 — 3 configuraciones

| Métrica | Baseline (single) | OVERLAP=0.70 sim51 | **OVERLAP=0.50 sim54** | Δ% 0.50 vs base |
|---|---|---|---|---|
| Saturated | 42/72 | 6/72 | 25/72 | — |
| M1 SDN CV | 1.192 | **0.357** ✓ | **0.846** | **-29.0 %** ❌ (req -30) |
| M3 SDN sum | 2382 | 6774 ✓ | 5362 | +125 % ✓ |
| M5 starv | 58.3 % | 8.3 % ✓ | 34.7 % | **-40.5 %** ❌ (req -50) |

**Conclusión mesh 3x3**: con OVERLAP=0.50, mesh **DEGRADA y deja de cumplir R-017**. M1 -29% (a 1pt de -30%), M5 -40.5% (a 9.5pt de -50%). Pasa de R-017 plenamente cumplido a R-017 NO cumplido.

### Causa probable

En mesh densa hay K=2-3 paths cortos por commodity. Threshold 0.70 (permisivo) acepta más paths → SDN tiene más opciones para balancear → fairness máxima. Threshold 0.50 filtra demasiados paths → menos opciones → SDN concentra tráfico en menos caminos → más heterogeneidad.

## Hipótesis para random/bridge

A pesar de degradar mesh, lanzo random porque la dinámica en sparse PUEDE ser distinta:
- En random sparse hay menos paths totales. Threshold 0.50 elimina paths casi-duplicados → SDN sólo asigna a paths realmente disjuntos → distribución posiblemente más uniforme.
- Si random cumple R-017 con 0.50 (esto requeriría algo especial dado que con 0.70 no cumplía), entonces 0.50 podría ser bueno para sparse aunque malo para mesh densa.

## random n=20 d=3 LANZADO

```bash
python3 -m tests.cli.dkms_topo random -n 20 -d 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 70 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_025/post/mediana_operador
```

- PID **3024929** (nohup, disowned).
- Output `/tmp/iter025-random-sdnv82.out`.
- node-id-offset 70 → node_ids 71-90 → local_qkc_id 100071-100090 (libre confirmado BD).
- Sim previa `random-n20-d3.0-srnd` (id=52) borrada con --force.
- ETA fin sim ~17 min (timeout 600s + loadtest 225s).

## Plan post-random

Si random cumple R-017 con 0.50, lanzar bridge. Si random no cumple, NO lanzar bridge (mesh ya falló, esto sería 0/3 + random falla = imposible 2/3). En ese caso cerrar OBJ-027 como "no cumple" y arrancar OBJ-028 con parámetro distinto (K=2 paths).

## Bloqueos

Ninguno. Próximo cron evalúa random.
