# iter_011 — OBJ-014 (orchestator EKS apunta a v8)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-014.

## Estado pre-cambio observado

Contexto EKS: `arn:aws:eks:eu-north-1:.../cluster/dkms1` ✓
Port-forwards: authz:200, orch:404 (servidor vivo) ✓.

**Imágenes en deploy ANTES:**
- `SDN_IMAGE=pablopio/sdn:v7`
- `ORR_IMAGE=pablopio/orr:v7`
- `QKC_IMAGE=pablopio/qkc:v7`
- (`DKMS_IMAGE=pablopio/dkms:v9` y `QUDITTO_IMAGE=pablopio/quditto:v7` — no se tocan, fuera de scope)

> Nota: la nota de `.memory.md` previa decía rollback path `sdn:v7/orr:v2/qkc:v4`. **Corrección:** el deploy real usaba `v7` en los 3. Rollback path correcto: `sdn:v7/orr:v7/qkc:v7`. Las imágenes `v7` están en registry remoto confirmado.

## Cambio aplicado

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    SDN_IMAGE=pablopio/sdn:v8 \
    ORR_IMAGE=pablopio/orr:v8 \
    QKC_IMAGE=pablopio/qkc:v8
```

Salida: `deployment.apps/orchestator env updated`.

## Rollout

`kubectl rollout status deploy/orchestator --timeout=120s` → `deployment "orchestator" successfully rolled out`. Tardó ~30s (1 old replica pending termination → terminated).

## Verificación post-rollout

- `kubectl describe deploy/orchestator | grep IMAGE`:
  - `SDN_IMAGE: pablopio/sdn:v8` ✓
  - `ORR_IMAGE: pablopio/orr:v8` ✓
  - `QKC_IMAGE: pablopio/qkc:v8` ✓
- Pod nuevo: `orchestator-78ddc6974b-2wwhq` Running 1/1, sin restarts.
- Port-forward del cliente se rompió (esperable tras rotación pod) → re-lanzado, pid 2582940. orch responde 404 a `/health` (servidor vivo, endpoint no existe — comportamiento legacy esperado).

Logs en `logs/{set_env.log, rollout_status.log, env_verification.log}`.

## R-015 verificación

`MULTIPATH_ENABLED` env var **NO** está set en el deploy. Comportamiento por default es **single-path legacy** — los pods nuevos cargan el código multipath pero NO lo activan hasta que OBJ-022 (Fase H) inyecte la env var. Esto es por diseño: OBJ-014 solo trae las imágenes nuevas a producción; la activación es Fase H.

## Rollback path verificado (R-010)

Si en cualquier momento queremos revertir:

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    SDN_IMAGE=pablopio/sdn:v7 \
    ORR_IMAGE=pablopio/orr:v7 \
    QKC_IMAGE=pablopio/qkc:v7
```

Las imágenes v7 siguen en docker.io/pablopio (confirmado en iter_010).

## Próximo

**Fase F arranca**: OBJ-015 — lanzar sim `mesh 3x3` en EKS con `MULTIPATH_ENABLED=true` para validación post-cambio. El env var se activa en el deploy de los pods del sim (no en orchestator global — el orchestator levanta sims con el env aplicado).

> Caveat técnico: hay que verificar dónde exactamente la env `MULTIPATH_ENABLED=true` se inyecta. El orchestator es Python; tiene que pasar la env al deploy de cada ORR de las sims que lanza. Si el código del orchestator no propaga env vars custom, OBJ-015 puede requerir setearla a nivel `deploy/orchestator` para que todos los sims hereden. Comprobaré en iter_012.

## Bloqueos

Ninguno. Fase E **cerrada 3/3**. Multi-path desplegado a producción (imágenes v8) pero por defecto OFF.
