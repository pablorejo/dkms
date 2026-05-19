# iter_030 — wait-state OBJ-028 sim 57 a 3m52s, snapshot MUY positivo K=2

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-028 (sim 57 corriendo, K=2).

## Estado actual

```
PID 3057513 ELAPSED=03:52 STAT=Sl
sim_id=57 ns=57 Active 3m48s
0/72 saturated (bootstrap)
saturation_timeout 600s → restante ~6 min
```

## Snapshot rate (5 DKMS de 9, 40 samples)

| DKMS    | peers | min | median | max | CV   |
|---------|-------|-----|--------|-----|------|
| dkms-940 | 8    | 54  | 90     | 119 | 0.24 |
| dkms-943 | 8    | 70  | 91     | 94  | 0.11 |
| dkms-946 | 8    | 73  | 91     | 148 | 0.23 |
| dkms-949 | 8    | 53  | 92     | 122 | 0.24 |
| dkms-952 | 8    | 73  | 91     | 95  | 0.12 |

**Agregado**: min=53.4, median=91.1, max=148.4, sum=3688 (5/9 DKMS).
- **CV preliminar = 0.203** (mucho menor que las anteriores!)
- **starvation = 0/40 = 0.0%**

## Comparativa mesh 3x3

| Config | CV | starv | max | R-017 |
|---|---|---|---|---|
| Baseline single | 1.192 | 58.3% | 79.4 | — |
| K=3 OVERLAP=0.70 (sim 51) | 0.357 ✓ | 8.3% ✓ | — | ✓ CUMPLE |
| K=3 OVERLAP=0.50 (sim 54) | 0.846 | 34.7% | — | ❌ |
| **K=2 OVERLAP=0.70 (sim 57)** prelim | **0.203** | **0.0%** | 148.4 | ¿muy probable cumplir? |

K=2 está dando **mejor M1 y M5 que K=3** en mesh densa. Si se mantiene al final:
- vs baseline: M1 -83%, M5 -100% → cumplir R-017 con margen aún mayor que sim 51.

## Hipótesis confirmada (preliminar)

K=2 reduce competencia interna por capacidad: con sólo 2 paths por commodity, el SDN no apila rate en commodities con paths muy disjuntos → distribución más uniforme.

Si esto también funciona en random sparse (donde con K=3 max sube a 470-767 kps por commodity rico), debería bajar drásticamente esa concentración → CV menor → M1 cumpliría.

## Decisión

- **NO interrumpir** sim 57.
- ETA fin total ~17 min desde inicio (estamos a ~4 min).
- Próximo cron evalúa sim 57 terminada (~14 min más).

## Bloqueos

Ninguno. Espera natural.
