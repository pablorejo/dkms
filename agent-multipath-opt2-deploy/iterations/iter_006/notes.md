# iter_006 — OBJ-008 (tests unit QKC multipath)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-008.

## Qué se hizo

### Refactor: extracción de `resolve_next_hop`

La lógica duplicada entre `handle_local_send_inner` y `handle_incoming_inner` (decide next_hop según `header_qkc_mp`) se movió a una función helper sincrónica:

```rust
fn resolve_next_hop(
    incoming_header_qkc_mp: &[u8],
    dest_final: u32,
    routing_fallback: impl FnOnce(u32) -> Option<u32>,
) -> Result<(u32, Vec<u8>)>
```

- Si `incoming_header_qkc_mp` vacío → fallback routing table.
- Si decodifica OK → `(next_hop, rest)`.
- Si decode falla → fallback routing table + header limpio.
- Si fallback también falla (`None`) → `Err(QkcError::NoRoute(dest))`.

El logging `tracing::debug` queda en el helper (no en callers) para no duplicar mensajes.

### 6 tests unit nuevos en `qkc::relay::tests`

1. **`resolve_next_hop_multipath_pops_first_and_propagates_rest`** — path `[2,3,4]` → next=2, rest=`[3,4]`. Verifica con `wire::decode_qkc_path`.
2. **`resolve_next_hop_single_hop_leaves_empty_header`** — path `[4]` → next=4, header saliente vacío (convención wire).
3. **`resolve_next_hop_empty_header_falls_back_to_routing_table`** — header `&[]` → fallback con dest_final pasado correctamente.
4. **`resolve_next_hop_corrupt_header_falls_back`** — bytes `[0xff;5]` → fallback silencioso, header saliente limpio.
5. **`resolve_next_hop_no_route_returns_err`** — header vacío + fallback `None` → `Err(NoRoute)`.
6. **`resolve_next_hop_corrupt_header_and_no_route_returns_err`** — combinación de los 2 fallos → `Err(NoRoute)` también.

Los tests usan closures como `routing_fallback` (no necesitan QkcService completo) y `wire::encode_qkc_path` / `decode_qkc_path` para construir/inspeccionar paths.

## Verificación

- `cargo test -p qkc --release --lib` → **14 passed** (era 8 + 6 nuevos).
- `cargo clippy -p qkc --release --all-targets -- -D warnings` → verde.
- Downstream sin regresiones: sdn 71, orr 75, wire 21, common 17.

## Fase C cerrada (3/3)

- OBJ-006 (handle_local_send): iter_005.
- OBJ-007 (handle_incoming): iter_005.
- OBJ-008 (tests unit): **iter_006**.

## Próximo

Fase D — Build local + smoke:
- OBJ-009: `cargo build --workspace --release` + tests workspace + clippy. Capturar outputs.
- OBJ-010: `docker build` local de las 3 imágenes nuevas.
- OBJ-011: smoke local (docker-compose o cargo run) con `MULTIPATH_ENABLED=true`.

## Bloqueos

Ninguno.
