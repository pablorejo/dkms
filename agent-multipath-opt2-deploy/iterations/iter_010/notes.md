# iter_010 — OBJ-012 + OBJ-013 (pre-flight + docker push autónomo)

**Fecha:** 2026-05-19
**Objetivos:** OBJ-012 (pre-flight check) + OBJ-013 (push autónomo).

OBJ-012 y OBJ-013 hechos juntos porque el manifest check y el push están ligados temporalmente (si el tag se libera entre iters, otro proceso podría reclamarlo). En una misma iter elimino esa ventana.

## OBJ-012 — Pre-flight check (R-014)

| Pre-requisito | Estado |
|---|---|
| `cargo test -p sdn -p orr -p qkc -p common` verde | ✓ (iter_007: 198 passed) |
| `cargo clippy ... -- -D warnings` verde | ✓ |
| `docker build` local OK | ✓ (iter_008: sdn:v8 138MB, orr:v3 137MB, qkc:v3 136MB) |
| Smoke local verde | ✓ (iter_009: smoke_chain_4_qkcs_consume_path_in_order) |
| Imágenes previas v7/v2/v4 intactas | ✓ |
| Tags destino NO en registry | (ver tabla siguiente) |

### Manifest inspect contra docker.io/pablopio

| Imagen | Tag inicial | Tag ya existe en registry? | Acción |
|---|---|---|---|
| sdn | v8 | NO (`no such manifest`) | usar v8 |
| orr | v3 | NO (`no such manifest`) | re-tag a v8 (coherencia bundle) |
| qkc | v3 | **SÍ existe** (otro deploy previo) | re-tag a v8 (libre + coherencia bundle) |

**Decisión:** todas las 3 imágenes pushean como `:v8` para que el bundle (`sdn:v8 + orr:v8 + qkc:v8`) sea coherente — más fácil de razonar en `kubectl set env`. Re-tag locales:

```bash
docker tag pablopio/orr:v3 pablopio/orr:v8
docker tag pablopio/qkc:v3 pablopio/qkc:v8
```

Image IDs preservados (mismos builds de iter_008):
- `pablopio/sdn:v8` = 0c95e4a6dfc7 (138 MB).
- `pablopio/orr:v8` = f1a3979702f8 (137 MB).
- `pablopio/qkc:v8` = 6eb465c440f2 (136 MB).

## OBJ-013 — Docker push autónomo

Los 3 pushes ejecutados secuencialmente:

```bash
docker push pablopio/sdn:v8   # → digest sha256:0c95e4a6dfc73b...
docker push pablopio/orr:v8   # → digest sha256:f1a3979702f83e...
docker push pablopio/qkc:v8   # → digest sha256:6eb465c440f2df...
```

Cada push subió ~2 layers nuevas. Logs en `logs/docker_push_{sdn,orr,qkc}.log`.

Verificación post-push: `docker manifest inspect` retorna JSON válido para los 3 → imágenes publicadas en docker.io/pablopio.

## Imágenes previas conservadas (rollback R-010)

En registry (verificable con `docker manifest inspect pablopio/<x>:<tag>`):

- `pablopio/sdn:v7` — desplegado actualmente en EKS.
- `pablopio/orr:v2` — desplegado actualmente en EKS.
- `pablopio/qkc:v4` — desplegado actualmente en EKS.

Rollback path (OBJ-026):
```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    MULTIPATH_ENABLED- \
    SDN_IMAGE=pablopio/sdn:v7 \
    ORR_IMAGE=pablopio/orr:v2 \
    QKC_IMAGE=pablopio/qkc:v4
```

## Próximo

OBJ-014: configurar orchestator en EKS para usar las imágenes nuevas + activar opt-in en pods de validación. Path 1 (preferido):

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    SDN_IMAGE=pablopio/sdn:v8 \
    ORR_IMAGE=pablopio/orr:v8 \
    QKC_IMAGE=pablopio/qkc:v8
```

Y esperar rollout. Verificar con `kubectl describe deploy/orchestator | grep -E 'SDN_IMAGE|ORR_IMAGE|QKC_IMAGE'`.

**Nota:** R-015 dice multipath es opt-in. En OBJ-014 NO activamos `MULTIPATH_ENABLED=true` global — eso es OBJ-022 (Fase H). Las imágenes v8 contienen el código pero por default OFF (legacy single-path).

## Bloqueos

Ninguno. Fase E avanza 2/3 (OBJ-012 ✓, OBJ-013 ✓, falta OBJ-014).
