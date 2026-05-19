# iter_031 — sim 57 CUMPLE R-017 mesh3x3 + lanzar random con K=2

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo:** seguir OBJ-028 (K=2). mesh3x3 cerrada cumpliendo; lanzar random.

## sim 57 mesh3x3 (K=2) — CIERRE CUMPLE R-017 ✓

### Métricas finales

| Métrica | Baseline | sim 57 (K=2) | Delta | R-017 req | Cumple |
|---|---|---|---|---|---|
| Saturated | 42/72 | 18/72 | — | — | — |
| M1 CV | 1.192 | **0.632** | **-47.0 %** | ≥-30 % | **✓** |
| M3 sum | 2382 | 5788 | +143 % | no caer >10 % | **✓** |
| M5 starv | 58.3 % | **25.0 %** | **-57.1 %** | ≥-50 % | **✓** |
| max rate | 79.4 | 165.4 | +108 % | — | — |
| Loadtest | n/a | 2163/2163 = 100 % p95=? | — | ≥95 % | ✓ |

### Comparativa todas las configuraciones mesh 3x3

| Config | CV | sum | starv | R-017 |
|---|---|---|---|---|
| Baseline single-path | 1.192 | 2382 | 58% | — |
| K=3 OVERLAP=0.70 (sim 51) | **0.357** | **6774** | **8.3%** | **✓ MEJOR margen** |
| K=3 OVERLAP=0.50 (sim 54) | 0.846 | 5362 | 34.7% | ❌ |
| **K=2 OVERLAP=0.70 (sim 57)** | 0.632 | 5788 | 25.0% | **✓ cumple** |

K=2 cumple R-017 en mesh aunque K=3+0.70 sigue siendo mejor. Pero el objetivo es **cumplir las 3 topologías** — si K=2 también arregla random/bridge, gana globalmente.

## Lanzar random n=20 d=3 con K=2

```bash
python3 -m tests.cli.dkms_topo random -n 20 -d 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 121 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_031/post/mediana_operador
```

PID **3079726**. Output `/tmp/iter031-random-sdnv83.out`. Offset 121 (node_ids 122-141, local_qkc_id 100122-100141 libres verificado BD).

ETA fin sim ~17 min.

### Hipótesis K=2 en random sparse

En random con K=3 + OVERLAP=0.70 (sim 52): M1 +97% (CV 1.187), max=470 kps. El SDN apilaba mucho en commodities con 3 paths realmente disjuntos.

Con **K=2**: el SDN sólo busca 2 paths por commodity, eliminando el tercer path "duplicado". Menos opciones → menor concentración de rate → CV menor.

**Predicción**: M1 random con K=2 mejor que K=3 (CV < 1.187). Si baja a ≤0.422 (≥-30%) → R-017 cumple.

## Bloqueos

Ninguno. Próximo cron evalúa sim random.
