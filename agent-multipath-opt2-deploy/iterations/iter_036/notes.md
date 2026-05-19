# iter_036 — OBJ-028 CIERRA fallido + OBJ-029 INICIA (cap por commodity)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.

## OBJ-028 CERRADO — NO cumple R-017

Tras 3 sims con K=2 + OVERLAP=0.70:

| Topo | M1 | M3 | M5 | R-017 |
|---|---|---|---|---|
| mesh3x3 (sim 57) | -47.0 % ✓ | +143 % ✓ | -57.1 % ✓ | **CUMPLE** |
| random (sim 58) | **-14.2 % ❌** | +10.0 % ✓ | -62.5 % ✓ | NO cumple |
| bridge (sim 59) | **+6.8 % ❌** | +0.1 % ✓ | **0 % ❌** | NO cumple |

### Agregado por criterio

- M1 ≥-30 %: **1/3** (sólo mesh) → req ≥2/3 → **❌**
- M3 no caer >10 %: 3/3 → **✓**
- M5 ≥-50 %: 2/3 (mesh+random) → **✓**

**M1 sigue siendo el cuello**. Hallazgo importante: bridge K=2 dio EXACTAMENTE el mismo resultado que sim 53 K=3 (CV 0.933, max 59.6, starv 42.9 %, sum 1589). **K no afecta bridge porque no hay paths alternativos para cross-cluster**. Bridge es intrínsecamente incompatible con multipath en su forma actual.

## OBJ-029 INICIA (último retry R-016, auto-tune iter 3)

### Cambios código `sdn/src/mcf.rs`

1. **K=3 restaurado**: revertido `McfSolver::default().k_paths = 2 → 3`. K=3 era mejor para mesh densa.
2. **Cap por commodity introducido**: nueva const `DEFAULT_MAX_RATE_PER_COMMODITY_KPS: f64 = 200.0`.
3. Aplicado tras solver: si `total = Σ rates_per_path > 200`, escalar proporcionalmente para que sum = 200.

```rust
pub const DEFAULT_MAX_RATE_PER_COMMODITY_KPS: f64 = 200.0;
...
if total > DEFAULT_MAX_RATE_PER_COMMODITY_KPS {
    let scale = DEFAULT_MAX_RATE_PER_COMMODITY_KPS / total;
    for r in rates_per_path_per_c[c_idx].iter_mut() {
        *r *= scale;
    }
    total = DEFAULT_MAX_RATE_PER_COMMODITY_KPS;
}
```

### Hipótesis

Cap=200 kps es ~2× del max baseline (mesh 79, random 100, bridge 50). Evidencia:
- En sim 52 random K=3 0.70: max=470 kps por commodity. **Cap a 200 forzaría reducción 57%**.
- En sim 55 random K=3 0.50: max=767 kps. Cap forzaría reducción 74%.
- mesh3x3 sim 51 max=139 kps. Cap=200 NO afecta.
- bridge sim 53 max=59 kps. Cap=200 NO afecta.

**Predicción**:
- **mesh**: cap NO actúa (max<200). Resultado debería ser similar a sim 51 (CV 0.357, M3 +184%, starv 8%). CUMPLE.
- **random**: cap recortaría commodities ricos → M1 mejora (CV baja). M3 podría caer porque parte del throughput se recorta. M5 starvation podría aumentar si recorta demasiado.
- **bridge**: cap NO actúa. Resultado similar a sim 53 (no cumple — pero ya el bridge era caso intrínsecamente perdido).

Si mesh + random cumplen → 2/3 → R-017 global ✓.

### Verificación pre-push (R-014)

- cargo test sdn 71 passed ✓
- clippy sdn verde ✓
- docker build sdn:v8.4 138MB ✓
- tag nuevo, no sobrescribe :v8/:v8.2/:v8.3 ✓
- rollback intacto ✓

### Push autónomo

`docker push pablopio/sdn:v8.4`:
- exit=0
- digest: `sha256:17bc69144ba90cd4197d4eec2d071e2f12d2db144ba4f1de162f325994247d7e`

### Deploy

```bash
kubectl -n dkms-main-ns set env deploy/orchestator SDN_IMAGE=pablopio/sdn:v8.4
```

Rollout OK. Port-forwards relanzados (orch + authz).

### Sim mesh 3x3 LANZADA

```bash
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 17 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_036/post/pequena_densa
```

PID **3130835**. Output `/tmp/iter036-mesh3x3-sdnv84.out`. sim_id=60, ns=60. Offset 17 (libre verificado). ETA ~17 min.

## Bloqueos

Ninguno. Próximo cron evalúa sim 60.

## Status R-016 último retry

Este es el **3er y último intento** según R-016 modificada. Si no cumple R-017 → `Estado: BLOQUEADO_AUTOTUNE_AGOTADO`.
