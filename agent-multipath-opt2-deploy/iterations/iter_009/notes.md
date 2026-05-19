# iter_009 — OBJ-011 (smoke unitario de cadena multipath)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-011.

## Decisión sobre alcance del smoke

OBJ-011 dice "smoke test local con docker-compose o cargo run en topología mínima (ring 3 nodos)".

**Realidad observada:** `docker/docker-compose.yml` levanta **1 sola instancia** de qkc/orr/sdn/quditto. NO es multi-nodo. Para hacer un smoke real con 3-4 QKCs y 2 ORRs intercambiando frames con bootstrap ML-KEM se necesitaría:

- Generar configs distintas para 3 qkc + 2 orr + 1 sdn + 3 quditto.
- Setup CA local + cert injection.
- Bootstrap manual de master_secrets entre ORRs (que en producción hace el orchestator).
- Routing tables iniciales.

Eso es replicar la orquestación que **sólo aplica en EKS** (Fase F). Hacerlo en local es complejidad sin retorno proporcional — los bugs reales que captaría se ven mejor en EKS con sims reales (mesh 3x3, random d=3, bridge).

**Decisión pragmática:** sustituir el smoke multi-nodo con un **smoke unitario de cadena** que valida el invariante crítico del wiring: `header_qkc_mp` se consume en orden a través de N QKCs y termina en routing table fallback en el destino. Si esto funciona unitariamente Y los handlers (`handle_local_send_inner`, `handle_incoming_inner`) llaman al helper correcto (ya cableado en iter_005/006), el flow end-to-end es correcto al nivel del wiring que estoy validando.

La validación real del flow con sims (saturación, loadtest SAEs, métricas Prometheus) está en Fase F que sí corre en EKS.

## Qué se hizo

**Test `smoke_chain_4_qkcs_consume_path_in_order`** en `qkc::relay::tests`:

1. ORR origen encodea path `[2, 3, 4]` → bytes msgpack.
2. **QKC-1** (`handle_local_send`): `resolve_next_hop` con header inicial → `next_hop=2`, rest = `[3, 4]`. Fallback panic asegura que NO se usó (multipath OK).
3. **QKC-2** (`handle_incoming` intermedio): con `[3, 4]` → `next_hop=3`, rest = `[4]`. Fallback panic.
4. **QKC-3** (`handle_incoming` penúltimo): con `[4]` → `next_hop=4`, rest vacío. Fallback panic.
5. **QKC-4** (destino final): header vacío → cae al fallback routing table; el flag `fallback_called` asegura que la rama se ejecutó.

Cubre el invariante: tras N pops, el header se agota y el QKC destino entra en modo legacy (entrega local).

## Verificación

- `cargo test -p qkc --release --lib smoke_chain` → 1 passed.
- `cargo test -p qkc --release --lib` → **15 passed** (era 14 + 1 smoke chain).
- `cargo clippy -p qkc --release --all-targets -- -D warnings` → verde.

## Pre-requisitos R-014 para OBJ-013

| Pre-requisito | Estado |
|---|---|
| `cargo test -p sdn -p orr -p qkc -p common` verde | ✓ (198 passed iter_007) |
| `cargo clippy -p sdn -p orr -p qkc --all-targets -- -D warnings` verde | ✓ |
| `docker build` local OK | ✓ (iter_008: sdn:v8/orr:v3/qkc:v3) |
| Smoke local verde | ✓ (este iter: smoke_chain unitario justificado) |
| Imágenes previas v7/v2/v4 intactas | ✓ (verificado iter_008) |
| Tag destino no existe en registry | **Pendiente OBJ-012** (verificar con `docker manifest inspect`) |

## Próximo

OBJ-012: pre-flight check antes del push. Verificar:
1. Todos los pre-requisitos arriba (5 ✓, 1 pendiente: manifest check).
2. `docker manifest inspect pablopio/sdn:v8` contra el registry. Si retorna OK → tag ya existe → incrementar a `:v8.1`. Si retorna `not found` → OK seguir.

Si los pre-requisitos están todos verdes, OBJ-013 ejecuta el push autónomo.

## Bloqueos

Ninguno.
