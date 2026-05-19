# iter_019 — OBJ-016 cerrado [x] + OBJ-017 lanzado (bridge --cluster-n 4 --cluster-count 2)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivos:** cerrar OBJ-016 + lanzar OBJ-017.

## OBJ-016 CERRADO [x]

Sim 52 (random n=20 d=3, post-multipath) terminó:
- saturation timeout 600s → **30/380 saturated** (8 %)
- median_ratio (obs/theory) = 0.978
- loadtest 225s → **2240 requests, 100 % OK, 0 throttled, 0 errors**
- p95 latency 84.3 ms, p99 99.3 ms
- Stop sim limpio (PID muerto, ns 52 NotFound) tras "Read timed out" del cliente (bug conocido).

### Métricas R-017 random n=20 d=3 (post vs baseline iter_010)

| Métrica | Baseline iter_010 | Post sim 52 | Delta | R-017 req | Cumple |
|---|---|---|---|---|---|
| total commodities | 380 | 380 | — | — | — |
| saturated | 48 | 30 | (proxy débil) | — | — |
| **M1 SDN CV** | **0.603** | **1.187** | **+96.7 %** | ≥-30 % | **❌** |
| **M3 throughput SDN sum** | **12917 kps** | **12940 kps** | **+0.2 %** | no caer >10 % | **✓** |
| **M5 starvation (rate<1)** | **48/380 (12.6 %)** | **30/380 (7.9 %)** | **-37.5 %** | ≥-50 % | **❌** (cerca) |
| SDN max rate | 99.9 kps | 469.8 kps | +370 % | — | — |
| SAE loadtest éxito | n/a | 100 % (2240/2240) | — | ≥95 % | ✓ |

**random n=20 d=3 NO cumple R-017**.

### Análisis del fallo (descriptivo, NO auto-tune por R-016)

- **M3 cumple**: el throughput agregado se mantiene (+0.2 %).
- **M5 cumple casi**: starvation cae de 12.6 % a 7.9 % = -37.5 %, pero el umbral R-017 exige ≥-50 %.
- **M1 empeora drásticamente**: CV pasa de 0.603 a 1.187 (+96.7 %). Esto NO es lo esperado.

Causa probable observada en datos (sin acción): en random d=3, los nodos heterogéneamente conectados (algunos con muchas adyacencias, otros casi aislados) ven multipath repartiendo bien sus paths cortos. Pero a flujos por edges cuello (estrellas), multipath les asigna rates "preferidos" descomunalmente altos (max=469.8 kps vs baseline max=99.9 kps) mientras a otros les da 0 → varianza explota.

Esto sugiere que en random sparse, multipath está **funcionando** (cada commodity recibe alguna fracción) pero **menos equitativamente** que single-path en términos de CV. Es decir, multipath aprovecha capacidad libre en commodities ricos en paths, lo que parece "bueno" para M3 y M5 pero "malo" para M1.

**R-016 PROHIBE auto-tune**. No toco `DEFAULT_OVERLAP_THRESHOLD`, ni pesos QoS, ni K.

### Artefactos OBJ-016

En `agent-multipath-opt2-deploy/iterations/iter_016/post/mediana_operador/`:
- `sat_analysis.json` (~205 KB) + `loadtest_analysis.json`
- `data/`: 6 CSVs (generator_state, per_commodity, theory_rates, loadtest_metrics, loadtest_requests, loadtest_sae_timeline)
- `plots/`: 8 PNGs
- 20 `dkms-*.log` files (~90 MB total)
- `payload.json`

## OBJ-017 LANZADO

```bash
python3 -m tests.cli.dkms_topo bridge --cluster-n 4 --cluster-count 2 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 17 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir agent-multipath-opt2-deploy/iterations/iter_019/post/cuello_bridge
```

- PID **2644077** (nohup, disowned).
- Output: `/tmp/iter019-bridge-post.out`.
- Topología: bridge 2 clusters de 4 nodos = **8 DKMS totales**.
- Node-id-offset 17 → node_ids 18-25 → local_qkc_id 100018-100025 (libre confirmado en BD).
- Esperable: ~10-13 min total (8 DKMS = más rápido que random 20).

### Stake decisión final

Estado actual Fase F:
- mesh 3x3 (OBJ-015) ✓ cumple R-017 con margen enorme.
- random n=20 d=3 (OBJ-016) ❌ NO cumple R-017.
- bridge (OBJ-017) — pendiente.

**Para validar Fase F necesito 2/3 cumplir**. Mesh ya está; bridge tiene que cumplir.

Si bridge cumple → 2/3 → continuar a Fase G.
Si bridge NO cumple → 1/3 → `Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS`.

NO ajusto parámetros (R-016). Sólo mido.

## Bloqueos

Ninguno. Próximo cron verifica OBJ-017.
