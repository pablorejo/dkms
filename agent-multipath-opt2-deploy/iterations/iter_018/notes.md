# iter_018 — wait-state OBJ-016 (sim 52 random n=20 d=3 a 7m14s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-016 (sim 52 sigue corriendo).

## Estado actual

```
PID 2620945 ELAPSED=07:14 STAT=Sl
sim_id=52 ns=52 Active 7m
saturation progress: 16/380 commodities (~4.2%)
saturation_timeout 600s → restante ~3 min
```

## Progresión saturación (de output `dkms-topo`)

| Tiempo | saturated/380 |
|---|---|
| t ≤ 5 min | 0/380 |
| t ~ 5:30  | 6 |
| t ~ 6:00  | 8 |
| t ~ 6:30  | 10 |
| t ~ 6:45  | 14 |
| t ~ 6:55  | 15 |
| t ~ 7:00  | 16 |

Curva acelerando como esperado (bootstrap completo → rates estables → buffers llenando).

## Snapshot rate distribution (5 DKMS de 20, 95 samples)

| DKMS    | peers | min | median | max | sum kps | CV   |
|---------|-------|-----|--------|-----|---------|------|
| dkms-764 | 19   | 0   | 44     | 140 | 790     | 0.71 |
| dkms-767 | 19   | 0   | 20     | 21  | 347     | 0.35 |
| dkms-770 | 19   | 14  | 33     | 148 | 729     | 0.76 |
| dkms-773 | 19   | 20  | 44     | 75  | 895     | 0.42 |
| dkms-776 | 19   | 0   | 32     | 54  | 638     | 0.41 |

**Agregado (95 samples)**:
- min=0.0, median=30.8 kps, max=148.0 kps, sum=3400 kps (sólo 5/20 DKMS de origen)
- **CV = 0.655** (preliminar, sólo 5 DKMS)
- **starvation (<1 kps) = 5/95 = 5.3 %** (preliminar)

## Comparativa preliminar vs mesh 3x3 final (orientativa)

- mesh 3x3 final: CV 0.357, starvation 6/72 = 8.3 %.
- sim 52 mid-sim: CV 0.655, starvation 5.3 %.
- CV más alto pero sim aún no estabilizada (16/380 vs 72/72 de mesh3x3 final).
- Heterogeneidad mayor es esperable en random (no todos pares simétricos).

## Decisión iter_018

- **NO interrumpir** sim 52.
- **NO lanzar OBJ-017** en paralelo.
- Próximo cron (~00:30Z) coge sim ya terminada (timeout 600s + loadtest 225s = ~14 min total desde lanzamiento → ETA fin ~00:32Z).

## Bloqueos

Ninguno. Espera natural ~5-7 min.
