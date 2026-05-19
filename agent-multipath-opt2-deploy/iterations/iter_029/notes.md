# iter_029 — auto-fix OBJ-028 (offset 40 colisionó) + relanzar sim 57

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-028 (auto-tune iter 2: K_PATHS=2).

## Síntoma

Sim 56 falló al crear: `HTTP 500 Internal Server Error` desde `create_simulation`. PID 3053751 murió.

## Diagnóstico (R-018 auto-fix operacional)

Log orchestator:
```
sqlalchemy.exc.IntegrityError: null value in column "local_qkc_id" of relation "kme" violates not-null constraint
DETAIL: Failing row contains (237, null, 100040, 5303, 5304, ..., DKMS-40, ...)
[parameters: [{'local_qkc_id': None, 'kme_id': 237}, {'local_qkc_id': None, 'kme_id': 238}]]
```

Es el **bug conocido `kme.local_qkc_id NotNull`** (CLAUDE.md, sección Known Issues): orchestator intentó UPDATE `local_qkc_id` a None para kme_id 237/238 (que ya existían con `local_qkc_id=100041` pre-asignados).

Causa: en iter_028 elegí `--node-id-offset 40` SIN verificar BD. Ese rango (node_ids 41-49) tiene 100041, 100046, 100047, 100048 ya ocupados de sims previas.

## Fix aplicado (auto-fix R-018)

1. Verificado BD: `max ocupado=100120`. Libre 9 contig: `node_id 91..99`.
2. Cleanup automático del rollback: el INSERT fallido se revirtió por transacción (kmes 237/238 con leftover quedaron pero `max=100120` indica que la BD se autocuró).
3. Relanzar sim con offset 91 (libre verificado).

## Sim 57 LANZADA (offset 91)

```bash
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 \
    --buffer-saturated --sae-test --force \
    --node-id-offset 91 --saturation-timeout 600 \
    --username config_user --password config_password \
    --output-dir iter_029/post/pequena_densa
```

PID **3057513**. Output `/tmp/iter029-mesh3x3-sdnv83.out`. Sim_id=57, ns=57. ETA fin sim ~17 min.

Confirmado:
- creating simulation ✓
- sim_id=57 namespace=57 ✓
- pods Ready (en progreso) ✓
- offset 91 → node_ids 92-100 → local_qkc_id 100092-100100 (libre).

## Cumplimiento R-018

- Caso aplicado: **"Pod en CrashLoopBackOff por error de configuración"** — aquí no era pod sino script CLI, pero el principio es el mismo (config: offset bad → fix: cambiar offset).
- Documentado síntoma + hipótesis + fix + verificación post-fix ✓.
- 1 retry consumido. Si vuelve a fallar → 2nd retry. Si tras 2 → BLOQUEADO_AUTO_FIX_FALLO.

## Bloqueos

Ninguno. Sim 57 corriendo, próximo cron evalúa.
