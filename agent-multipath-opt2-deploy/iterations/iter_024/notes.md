# iter_024 — wait-state OBJ-027 (sim 54 mesh3x3 a 6m16s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-027 (sim 54 sigue corriendo).

## Estado actual

```
PID 3001586 ELAPSED=06:16 STAT=Sl
sim_id=54 ns=54 Active 6m11s
0/72 saturated (curva similar a sim 51 a 6 min)
saturation_timeout 600s → restante ~4 min
```

## Snapshot rate distribution (5 DKMS de 9, 40 samples)

```
dkms-850: peers=8, min=88, median=110, max=121, sum=860, CV=0.12
dkms-853: peers=8, min=14, median=107, max=114, sum=748, CV=0.36
dkms-856: peers=8, min=88, median=114, max=129, sum=910, CV=0.11
dkms-859: peers=8, min=14, median=94, max=294, sum=832, CV=0.81
dkms-862: peers=8, min=68, median=68, max=116, sum=680, CV=0.28

Aggregate: min=14.5, median=110.1, max=294.0, sum=4030 (5/9 DKMS)
CV preliminar = 0.415
starv (<1 kps) = 0/40 = 0.0%
```

## Comparativa preliminar

| Métrica | Baseline (single) | sim 51 (0.70) final | sim 54 (0.50) @ 6 min |
|---|---|---|---|
| CV | 1.192 | 0.357 | **0.415** (5/9 DKMS) |
| sum (sólo 5/9) | (72) 2382 kps | (72) 6774 kps | (40) 4030 kps |
| starvation | 58.3% | 8.3% | **0.0%** |

**Observación clave**: con threshold 0.50, **starvation cae a 0** vs 8.3% con 0.70. Esto sugiere que con threshold más estricto (paths más disjuntos), los flujos repartidos llegan más uniformemente — nadie queda en starvation total.

**Sin embargo**: CV preliminar 0.415 vs 0.357 de 0.70 (peor M1 spread). Causa probable: con threshold 0.50 se aceptan menos paths por commodity → menos opciones para balancear → distribución más bimodal entre commodities con paths cortos vs largos.

**Hipótesis hacia mediana_operador/cuello_bridge**:
- random sparse, donde 0.70 daba M1 +97% (peor que baseline): 0.50 puede mejorar **algo** porque exige paths más disjuntos, evitando que el SDN repita capacidad.
- bridge cuello: con 1 enlace inter-cluster, el threshold no cambia nada (sigue degenerando).

## Decisión

- **NO interrumpir** sim 54.
- ETA fin saturation ~00:35Z (timeout 600s).
- ETA fin total (con loadtest) ~00:42Z.
- Próximo cron ~00:40Z probablemente coja sim casi terminada.

## Bloqueos

Ninguno. Espera natural.
