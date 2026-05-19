# iter_042 — OBJ-029 CIERRA fallido + ESTADO: BLOQUEADO_AUTOTUNE_AGOTADO

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.

## OBJ-029 CIERRA — bridge sim 62 cap=200

| Métrica | Baseline | sim62 cap=200 | Delta | R-017 |
|---|---|---|---|---|
| Saturated | 24/56 | 24/56 | — | — |
| M1 CV | 0.874 | **0.933** | +6.8 % | ❌ |
| M3 sum | 1587 | 1589 | +0.1 % | ✓ |
| M5 starv | 42.9 % | 42.9 % | 0.0 % | ❌ |
| max | 49.6 | 59.6 | — | — |
| Loadtest | n/a | 2232/2232 = 100 % | — | ✓ |

**Bridge sim 62 IDÉNTICO a sim 53 (K=3) y sim 59 (K=2)**. Cap=200 NO actúa (max=59 < 200). Confirma: bridge es intrínsecamente incompatible con multipath en su forma actual (cuello único cross-cluster, sin paths alternativos).

## R-017 EVALUACIÓN FINAL OBJ-029 (cap=200)

| Topo | M1 | M3 | M5 |
|---|---|---|---|
| mesh (sim 60) | **-78.1 % ✓** | +195.9 % ✓ | **-95.2 % ✓** |
| random (sim 61) | **+23.4 % ❌** | +3.0 % ✓ | -64.6 % ✓ |
| bridge (sim 62) | +6.8 % ❌ | +0.1 % ✓ | 0.0 % ❌ |

- M1 ≥-30 % en ≥2/3: **1/3** ❌ (solo mesh)
- M3 no caer >10 % en 3/3: 3/3 ✓
- M5 ≥-50 % en ≥2/3: **2/3** ✓ (mesh + random)

**R-017 NO CUMPLE globalmente** porque M1 sólo en 1/3.

## Tabla resumen de TODAS las configs probadas (3 retries R-016)

| Config (push tag) | mesh M1 | mesh M3 | mesh M5 | random M1 | random M3 | random M5 | bridge M1 | bridge M3 | bridge M5 |
|---|---|---|---|---|---|---|---|---|---|
| K=3 OVERLAP=0.70 (sdn:v8 original) | -70.0% | +184% | -85.7% | +96.7% | +0.2% | -37.5% | +6.8% | +0.1% | 0% |
| K=3 OVERLAP=0.50 (sdn:v8.2) | -29.0% | +125% | -40.5% | +163.5% | +13.7% | -62.5% | — | — | — |
| K=2 OVERLAP=0.70 (sdn:v8.3) | -47.0% | +143% | -57.1% | -14.2% | +10.0% | -62.5% | +6.8% | +0.1% | 0% |
| K=3 cap=200 (sdn:v8.4) | **-78.1%** | **+196%** | **-95.2%** | +23.4% | +3.0% | -64.6% | +6.8% | +0.1% | 0% |

**Hallazgos clave**:

1. **mesh3x3 con cap=200 (sdn:v8.4) es el ÓPTIMO encontrado** — mejor M1, M3 y M5 que cualquier otra config.
2. **bridge es intrínsecamente incompatible** — cap NO actúa (max baseline=49 muy bajo), y no hay paths alternativos para flujos cross-cluster. Las 3 configs probadas dan idénticos números.
3. **random con K=2 (sin cap) tiene el mejor M1** (-14.2 %), no llega a -30 % pero es el más cercano. Cap=200 empeora M1 a +23.4 %.
4. **Trade-off real**: ningún parámetro single optimiza las 3 topologías a la vez. Cada cambio mejora unas y empeora otras.

## Decisión

Aplicando R-016 modificada:
- "Máximo 3 retries de auto-tune." → **3 retries gastados** (OBJ-027, OBJ-028, OBJ-029).
- "Si tras los 3 retries R-017 sigue sin cumplir → `Estado: BLOQUEADO_AUTOTUNE_AGOTADO`."

**Estado: BLOQUEADO_AUTOTUNE_AGOTADO**.

Fase G (OBJ-020, OBJ-021) y Fase H (OBJ-022..026) **NO se ejecutan**.

## Reporte para humano

### Configuración recomendada (si se decide aceptar 1/3 R-017)

`pablopio/sdn:v8.4` (cap=200 commodity) — la **mejor configuración encontrada** para mesh densa y aceptable para random/bridge.

```bash
kubectl -n dkms-main-ns set env deploy/orchestator SDN_IMAGE=pablopio/sdn:v8.4
```

### Conclusiones técnicas

- **Multipath funciona estructuralmente** (loadtest 100 % en 12+ sims, no panics ni errors).
- **Multipath sólo aporta valor en topologías densas con K paths cortos viables** (mesh).
- **Random sparse**: SDN distribuye rate desigualmente entre commodities; ningún cap o filtro lo iguala completamente.
- **Bridge cuello**: sin paths alternativos = sin multipath.

### Acciones recomendadas (manual, fuera de agente)

1. Revisar el solver MCF para añadir **fair-share entre commodities** (no solo entre paths del mismo commodity).
2. O bien: aceptar que multipath sólo se activa por topología (heuristica de "topología densa" → multipath ON; else OFF).
3. O bien: redefinir R-017 con criterios distintos (no CV de SDN rates).

### Rollback path

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    ORR_MULTIPATH_ENABLED- \
    SDN_IMAGE=pablopio/sdn:v7 \
    ORR_IMAGE=pablopio/orr:v7 \
    QKC_IMAGE=pablopio/qkc:v7
```

Revierte a single-path baseline.

## Bloqueos

**Estado: BLOQUEADO_AUTOTUNE_AGOTADO**.
