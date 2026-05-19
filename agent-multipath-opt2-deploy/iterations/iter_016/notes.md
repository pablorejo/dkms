# iter_016 — OBJ-015 cerrado + OBJ-016 lanzado (sim random n=20 d=3)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivos:** cierre OBJ-015 [x] + lanzar OBJ-016 (random n=20 d=3 post-cambio).

## OBJ-015 CERRADO [x]

Sim 51 (mesh 3x3 post-multipath) terminó limpiamente:
- saturation timeout 600s → 6/72 commodities saturados.
- loadtest 225s → **2262 requests, 100 % OK, 0 throttled, 0 errors**.
- p95 latency 49.8 ms, p99 92.5 ms.
- Stop sim limpio (PID muerto, ns 51 NotFound) — "Read timed out" del cliente es bug conocido CLAUDE.md, el stop server-side sí ocurrió.

### Numeros comparativa contra baseline iter_009 (calculados desde sat_analysis.json)

| Métrica | Baseline single-path | Post multipath | Delta | R-017 req | Cumple? |
|---|---|---|---|---|---|
| total commodities | 72 | 72 | — | — | — |
| saturated count | 42/72 (58 %) | 6/72 (8 %) | — | — | (proxy débil — ver nota) |
| **M1 SDN coeff var** (stdev/mean) | **1.192** | **0.357** | **-70.0 %** | ≥-30 % | ✓✓✓ |
| **M3 throughput SDN sum** | **2382 kps** | **6774 kps** | **+184 %** | no caer >10 % | ✓✓✓ |
| **M5 starvation** (rate<1 kps) | **42/72 (58 %)** | **6/72 (8 %)** | **-86 %** | ≥-50 % | ✓✓✓ |
| SDN median rate | 0.0 kps | 101.0 kps | +∞ | — | — |
| SDN p25/p75 rate | (0, 0) | (82.7, 115.5) | reparto equitativo | — | — |
| SDN max rate | 79.4 kps | 139.5 kps | +75.7 % | — | — |
| median ratio obs/theory | 0.949 | 1.654 | +74 % | — | — |
| **SAE loadtest éxito** | n/a | **100 % (2262/2262)** | — | ≥95 % | ✓ |

**mesh 3x3 cumple R-017 con margen enorme**: M1 -70 %, M5 -86 %, M3 +184 % (sube, no cae). Loadtest 100 %.

### Por qué saturated count cayó pero M3 subió

Single-path baseline: SDN elegía un camino único; 42 commodities se llenaban (los que pasaban por edges no saturados) y 30 quedaban a 0 kps (starved). Total kps = 2382.

Multipath post: SDN reparte rate entre K caminos paralelos. CADA commodity recibe ~100 kps de promedio (vs 0 en muchos baseline). Cap individual (65536/100=655 s = ~11 min) no llega a saturar en 600s timeout. Pero el **sistema entera procesa 2.85× más keys/s**.

El criterio de "saturated count" era buen proxy para single-path; **para multipath el criterio correcto es M3 throughput sum + M1 spread + M5 starvation**, que son las R-017.

### Artefactos OBJ-015

Todos en `agent-multipath-opt2-deploy/iterations/iter_013/post/pequena_densa/`:
- `sat_analysis.json` (39 KB) — métricas saturation
- `loadtest_analysis.json` — métricas SAE test
- `data/`: 6 CSVs (generator_state, per_commodity, theory_rates, loadtest_metrics, loadtest_requests, loadtest_sae_timeline)
- `plots/`: 8 PNGs (4 saturation + 4 loadtest)
- `dkms-{736..760}-*.log` (9 logs, 50 MB total)
- `payload.json` (topología enviada al orchestator)

## OBJ-016 LANZADO

```bash
python3 -m tests.cli.dkms_topo random -n 20 -d 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 131 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir agent-multipath-opt2-deploy/iterations/iter_016/post/mediana_operador
```

- PID **2620945** (nohup, disowned).
- Output: `/tmp/iter016-random20d3-post.out`.
- Topología: random 20 nodos, grado medio 3 → ~30 edges, ~380 commodities (20×19).
- Node-id-offset 131 → node_ids 132-151 → `local_qkc_id` 100132-100151 (libre verificado en BD; max ocupado=100130).
- Pre-flight: orchestator deploy con `SDN_IMAGE=pablopio/sdn:v8 ORR_IMAGE=pablopio/orr:v8.1 QKC_IMAGE=pablopio/qkc:v8 ORR_MULTIPATH_ENABLED=true` ✓.
- Esperable: ~15-20 min total (bootstrap ~3 min + saturate ~10 min + loadtest ~5 min).
- Próximo cron (00:23Z aprox) → verificar progreso. Esperable terminación ~00:30Z.

## Decisión iter_016

Una iter = una unidad de trabajo. Aquí hago 2 en realidad pero son atómicas:
1. **Cerrar OBJ-015** (analizar artefactos producidos, marcar [x], registrar números).
2. **Lanzar OBJ-016** (background, no bloquea iter).

No analizo OBJ-016 todavía (sim recién arrancada). Próximo cron lo coge.

NO lanzo OBJ-017 (bridge) en paralelo — viola R-007 mental (dos sims pesadas en EKS).

## Bloqueos

Ninguno. Sim OBJ-016 corriendo. Próximo cron evalúa.
