# iter_033 — wait-state OBJ-028 (sim 58 random K=2 a 6m03s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-028 — sim 58 sigue corriendo.

## Estado

```
PID 3079726 ELAPSED=06:03 STAT=Sl
sim_id=58 ns=58 Active 5m53s
4/380 saturated (bootstrap+early sat)
saturation_timeout 600s → restante ~4 min
```

## Snapshot (5/20 DKMS, 95 samples)

| DKMS    | peers | min | median | max | CV   |
|---------|-------|-----|--------|-----|------|
| dkms-1001 | 19  | 3   | 54     | 305 | 1.07 |
| dkms-1004 | 19  | 3   | 44     | 98  | 0.64 |
| dkms-1007 | 19  | 22  | 49     | 305 | 0.97 |
| dkms-1010 | 19  | 22  | 29     | 107 | 0.62 |
| dkms-1013 | 19  | 33  | 43     | 73  | 0.22 |

**Agregado**: min=3.4, median=42.8, max=305.4, sum=5108 (5/20 DKMS).
- **CV preliminar = 0.888**
- **starvation = 0.0%**

## Análisis preliminar comparativa random n=20 d=3

| Config | CV | starv | max | M1 vs baseline |
|---|---|---|---|---|
| Baseline single | 0.603 | 12.6% | 99.9 | — |
| K=3 OVERLAP=0.70 (sim52) | 1.187 | 7.9% | 469.8 | +97% ❌ |
| K=3 OVERLAP=0.50 (sim55) | 1.589 | 4.7% | 767.6 | +163% ❌ |
| **K=2 OVERLAP=0.70 (sim58)** prelim | **0.888** | **0.0%** | 305.4 | +47% ❌ (sigue PEOR que baseline pero menos malo) |

K=2 + OVERLAP=0.70 mejora vs K=3 (CV 0.888 < 1.187), pero todavía empeora vs baseline (+47%). Para cumplir M1 R-017 (-30%) necesitaría CV ≤ 0.422 — improbable que el final sea tan bajo.

## Hipótesis post-resultado final

Predicción:
- sim 58 final probablemente: CV ~1.0-1.2 (similar al snapshot), starv 4-8%, max 300-500 kps.
- **M1 random NO cumplirá** con K=2.
- **M5 random posiblemente cumple** (-50% vs baseline 12.6% = post ≤6.3%; snapshot ya en 0%).
- M3 probablemente cumple (no caer >10%).

Si M1 falla → sim 58 no cumple R-017 ✗.
- Estado actual de R-017 con K=2: mesh3x3 cumple (sim 57), random falla (predicho sim 58).
- bridge probablemente similar a antes (no mejora con multipath en cuello único).
- → 1/3 cumple → R-017 global falla.

## Decisión

- **NO interrumpir** sim 58.
- Si confirma fallo M1 → cerrar OBJ-028 fallido → arrancar OBJ-029 con cap por commodity.

## Bloqueos

Ninguno. Espera natural.
