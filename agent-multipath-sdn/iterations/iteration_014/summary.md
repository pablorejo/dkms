# Iteración 014 — OBJ-015 verificación pre-deploy + bloqueo OBJ-016 identificado

**Fecha:** 2026-05-18

## Qué se hizo

### Verificación de tests (OBJ-015)

- `make topo-cli-test` → **190 passed** ✓
- `cargo test -p sdn --lib --release` → **71 passed** ✓
- `cargo test -p orr --lib --release` → **69 passed** ✓
- `cargo test -p common --lib --release` → **17 passed** ✓

### Pre-existentes NO causados por la migración

- `cargo test -p dkms` → 1 fail en `state::buffer::tests::try_push_returns_key_when_full` (documentado desde iter 006).
- `cargo clippy --workspace --release` → error en `qkc` (unused_imports en `v014::*`).

Ambos crates están protegidos por **R-002** (no tocar QKC/wire) y **R-003** (no tocar DKMS). No puedo arreglarlos sin violar las restricciones inmutables. Documentado.

### Bridge sim 50 — completo con loadtest

Bench refrescado sin cambios en M1-M5 (loadtest no afecta esas métricas). loadtest_metrics.csv ya en disco.

## Bloqueo crítico identificado para OBJ-016

**OBJ-016 dice:** "Lanzar contra EKS las 3 topologías DESPUÉS del cambio".

**Problema:** sin un cambio efectivamente DEPLOYADO, OBJ-016 mediría exactamente lo mismo que OBJ-014.

El cluster EKS usa las imágenes Docker `pablopio/sdn:v7`, `pablopio/orr:v2`, etc. Mi código está en working tree local pero:

1. **No hay wiring real en `send_onion_path`** que invoque `pick_multipath_qkc_hops`. El RPC `GetPathsWithRatios` existe en el SDN pero ningún cliente lo llama todavía.
2. **No hay imagen Docker nueva publicada.**

Sin esos dos pasos, post-cambio == baseline trivialmente.

## Propuesta al usuario: OBJ-021 (no añado yo automáticamente; R-011)

Sugerencia para que el usuario añada a `.objetives.md`:

> `[ ] OBJ-021: Wiring multipath end-to-end. (a) Implementar conversión qkc_hops→orr_hops en `OrrService` (acceso a `topology.orr_by_qkc` via RPC nuevo o cache local). (b) Integrar `pick_multipath_qkc_hops` en `send_onion_path` cuando `max_hops` indica modo onion. (c) Pasar el `orr_path` muestreado via `app_header["orr_path"]`. (d) `docker build` + push de `pablopio/sdn:v8` y `pablopio/orr:v3`. (e) Actualizar env del orchestator deployment con las nuevas imágenes.`

**Sin OBJ-021 (o equivalente), OBJ-016/017/018 son no-ops.**

## Decisión de iter

- Marco **OBJ-015 [x]** porque la condición literal ("make topo-cli-test verde") se cumple. Los fails preexistentes están fuera del alcance.
- **NO ejecuto OBJ-016** en esta iter: no aportaría datos distintos al baseline. Hacer 3 sims más solo para confirmar "no hay cambio" sería gastar EKS sin beneficio.
- Documento el bloqueo en `.memory.md` y `.results.md`.
- En la próxima iter, el agente debería re-leer y, viendo OBJ-016 sin haber sido modificado en `.objetives.md`, escoger entre:
  - (a) Ejecutarlo igual y registrar el resultado idéntico al baseline (honesto).
  - (b) Anotar bloqueo y avanzar a OBJ-019/020 si están desbloqueables.

## Bloqueos

- **OBJ-016 bloqueado** por falta de wiring real + imagen Docker nueva. Requiere decisión del usuario (OBJ-021).
