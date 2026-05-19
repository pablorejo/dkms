# iter_040 — sim 61 random NO cumple + lanzar bridge (completar OBJ-029)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-029 (cap=200, último retry R-016).

## sim 61 random cap=200 — RESULTADO FINAL

| Métrica | Baseline | sim52 (sin cap) | sim58 (K=2) | **sim61 (cap=200)** | R-017 |
|---|---|---|---|---|---|
| Saturated | 48/380 | 30/380 | 18/380 | 16/380 | — |
| **M1 CV** | 0.603 | 1.187 | 0.517 | **0.745** | **+23.4 %** ❌ |
| M3 sum | 12917 | 12940 | 14210 | 13300 | +3.0 % ✓ |
| M5 starv | 12.6 % | 7.9 % | 4.7 % | **4.5 %** | **-64.6 %** ✓ |
| max rate | 99.9 | 469.8 | 151.3 | 170.9 | — |

Interesante: max=170.9 < cap=200. Cap NO actuó agresivamente (con K=3 sin OVERLAP modificado, el solver no estaba apilando tanto como predicción). Aún así CV 0.745 > umbral 0.422.

**M1 NO cumple** (-30 % requerido, post +23 %). M3 ✓, M5 ✓.

## Estado actual OBJ-029

- mesh3x3 sim60 cap=200: **CUMPLE ✓✓✓** (M1 -78 %, M3 +196 %, M5 -95 %).
- random sim61 cap=200: NO cumple M1 (M3 + M5 ✓).
- bridge sim ?: pendiente.

Para completar el protocolo R-016 (retry incluye 3 sims), lanzo bridge.

## bridge --cluster-n 4 --cluster-count 2 LANZADO

```bash
python3 -m tests.cli.dkms_topo bridge --cluster-n 4 --cluster-count 2 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 75 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_040/post/cuello_bridge
```

PID **3173835**. Output `/tmp/iter040-bridge-sdnv84.out`. Offset 75 libre. ETA ~13 min.

### Predicción bridge cap=200

En sim 53 y sim 59 (K=3 y K=2 sin cap) bridge dio EXACTAMENTE el mismo resultado: CV=0.933, max=59.6, starv=42.9 %. Bridge es intrínsecamente incompatible con multipath (cuello único cross-cluster).

Con cap=200: max=59 < 200 → cap NO actúa → predicción CV=0.933 ❌ (idéntico).

## Stake final OBJ-029

Si bridge NO cumple M1 (lo más probable):
- mesh ✓, random ❌, bridge ❌ → M1 1/3 ❌.
- 3 retries gastados sin cumplir R-017.
- **`Estado: BLOQUEADO_AUTOTUNE_AGOTADO`**.

Si bridge SÍ cumple M1 (improbable):
- mesh ✓, random ❌, bridge ✓ → M1 2/3 ✓.
- M3 verificar. M5 verificar.
- Si M3 y M5 también 2/3 → CUMPLE → Fase G.

## Bloqueos

Ninguno. Próximo cron evalúa bridge.
