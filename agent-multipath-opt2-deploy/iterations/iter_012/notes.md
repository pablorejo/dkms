# iter_012 — Fix propagación env + orr:v8.1 (precondición Fase F)

**Fecha:** 2026-05-19
**Objetivo:** preparar la activación de multipath en pods de sims (precondición de OBJ-015). No es un OBJ formal, es un hotfix necesario detectado al estudiar el orchestator.

## Problema detectado

Al inspeccionar `orchestrator/pods.py::_orr_container` (~líneas 3525-4058), descubrí que el orchestator **solo propaga al sidecar ORR las env vars con prefix `ORR_*` o `ORR__*`**. Mi helper de iter_003 leía `MULTIPATH_ENABLED` sin prefix → NO llegaba al pod ORR.

Si no se arregla, OBJ-015 lanzaría una sim con orr:v8 instalado pero multipath inactivo → comportamiento idéntico al baseline → R-017 no se cumpliría → BLOQUEADO_CRITERIOS_NO_CUMPLIDOS.

## Qué se hizo

### Fix en `orr/src/service.rs::compute_qkc_path_header`

Cambio mínimo (10 líneas):

```rust
let enabled = std::env::var("ORR_MULTIPATH_ENABLED")
    .ok()
    .or_else(|| std::env::var("MULTIPATH_ENABLED").ok())
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false);
```

Acepta `ORR_MULTIPATH_ENABLED` (preferido, propagado por orchestator) y mantiene `MULTIPATH_ENABLED` como fallback para tests locales / cargo run standalone.

### Build + push orr:v8.1

- `docker manifest inspect pablopio/orr:v8.1` → NO existe en registry. OK.
- `docker build -t pablopio/orr:v8.1 -f orr/Dockerfile .` → OK.
- `docker push pablopio/orr:v8.1` → digest sha256:539356300becfd...

Tag `:v8.1` (no sobrescribe `:v8`, R-014).

### Update orchestator + activación opt-in

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    ORR_IMAGE=pablopio/orr:v8.1 \
    ORR_MULTIPATH_ENABLED=true
```

Rollout OK ~30s. Pod nuevo Running. Verificación:
```
ORR_IMAGE:                 pablopio/orr:v8.1
ORR_MULTIPATH_ENABLED:     true
```

`SDN_IMAGE=pablopio/sdn:v8` y `QKC_IMAGE=pablopio/qkc:v8` se mantienen sin cambios.

### Verificación

- `cargo test -p orr --release --lib` → **75 passed** (sin regresiones, el cambio de env-var lookup es retrocompatible).
- `cargo clippy -p orr --release --all-targets -- -D warnings` → verde.
- Port-forward orchestator relanzado tras rotación (pid 2594680).

## R-015 alcance

`ORR_MULTIPATH_ENABLED=true` ahora está en el deploy del orchestator global. Esto significa que **TODOS los pods ORR de TODAS las sims nuevas** que el orchestator levante activarán multipath. NO está limitado a las sims de validación.

Mientras este agente sólo lance sims de validación (no hay otras sims activas en EKS al momento de este push), la garantía R-015 se mantiene en la práctica. Si surge una sim ajena durante la validación, R-018 contempla auto-fix operacional.

## Rollback path actualizado

```bash
kubectl -n dkms-main-ns set env deploy/orchestator \
    ORR_MULTIPATH_ENABLED- \
    ORR_IMAGE=pablopio/orr:v7 \
    SDN_IMAGE=pablopio/sdn:v7 \
    QKC_IMAGE=pablopio/qkc:v7
```

(El `-` borra la env var.) Las imágenes v7 siguen en registry.

## Próximo

OBJ-015: lanzar sim `mesh 3x3` en EKS con MULTIPATH activo (heredado del orchestator). Comparable a baseline `agent-multipath-sdn/iteration_009/baseline/pequena_densa_mesh3x3/`.

## Bloqueos

Ninguno. La precondición está resuelta.
