# iter_038 — mesh3x3 cap=200 ÉPICO + lanzar random

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-029.

## sim 60 mesh3x3 cap=200 — RESULTADO ÉPICO ✓✓✓

### Comparativa todas las configuraciones mesh 3x3

| Config | sat | CV | sum | starv | max |
|---|---|---|---|---|---|
| Baseline single | 42/72 | 1.192 | 2382 | 58.3% | 79.4 |
| K=3 OVERLAP=0.70 sim51 | 6/72 | 0.357 | 6774 | 8.3% | 139.5 |
| K=3 OVERLAP=0.50 sim54 | 25/72 | 0.846 | 5362 | 34.7% | 253.8 |
| K=2 OVERLAP=0.70 sim57 | 18/72 | 0.632 | 5788 | 25.0% | 165.4 |
| **K=3 cap=200 sim60** | **2/72** | **0.261** | **7048** | **2.8%** | **141.5** |

### R-017 mesh3x3 (sim 60 cap=200)

| Métrica | Baseline | sim 60 | Delta | R-017 req | Cumple |
|---|---|---|---|---|---|
| M1 CV | 1.192 | 0.261 | **-78.1 %** | ≥-30 % | **✓✓✓** |
| M3 sum | 2382 | 7048 | **+195.9 %** | no caer >10 % | **✓✓✓** |
| M5 starv | 58.3% | 2.8% | **-95.2 %** | ≥-50 % | **✓✓✓** |
| Loadtest | n/a | 2145/2145 = 100% | — | ≥95% | ✓ |

**mesh3x3 con cap=200 mejor que TODAS las configs anteriores**. Cap recortó commodities con paths muy disjuntos sin afectar throughput global (M3 sube +196%). M5 starvation casi se elimina (2.8 % vs 8.3 % sim51).

## random n=20 d=3 LANZADO

```bash
python3 -m tests.cli.dkms_topo random -n 20 -d 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 54 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_038/post/mediana_operador
```

PID **3152982**. Output `/tmp/iter038-random-sdnv84.out`. Offset 54 (node_ids 55-74, lqkc 100055-100074 libres). ETA fin sim ~17 min.

### Hipótesis random con cap=200

En sim 52 random K=3 0.70 sin cap: max=470 kps, CV=1.187 (+97%).
Con cap=200: max forzado a 200. Reducción 57% en commodities ricos.

Esperable:
- CV debería bajar drásticamente — predicción CV ≤ 0.6.
- starv probablemente baja (commodities ricos pierden capacidad → SDN puede redistribuir a pobres).
- M3 podría caer porque parte del throughput de commodities ricos se pierde. Cuestión es cuánto.

Si CV ≤ 0.422 → cumple M1 R-017. Si M3 no cae >10% → cumple M3.

## Estado actual OBJ-029

- mesh3x3 sim60: **CUMPLE R-017 con margen enorme** ✓✓✓
- random sim ?: pendiente
- bridge: no se lanza si random falla (mesh+bridge no son 2/3 si random falla M1; necesito random cumpla)

## Bloqueos

Ninguno. Próximo cron evalúa random.
