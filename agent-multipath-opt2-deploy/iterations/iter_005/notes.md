# iter_005 — OBJ-006 + OBJ-007 (QKC source routing consume path)

**Fecha:** 2026-05-19
**Objetivos:** OBJ-006 (handle_local_send) + OBJ-007 (handle_incoming).

OBJ-006 y OBJ-007 hechos en la misma iter porque ambos modifican `forward_plaintext`/`send_frame_to_peer` que requiere `header_qkc_mp` propagado. Separarlos rompería el build entre iters.

## Qué se hizo

### `qkc/src/relay.rs::handle_local_send_inner`

Antes de invocar `forward_plaintext`, mira `frame.header_qkc_mp`:
- **Vacío**: comportamiento legacy. `next_hop = routing.next_hop(dest)`, propaga `Vec::new()`.
- **No vacío**: intenta `wire::pop_qkc_path_next_hop`.
  - Ok((next_hop, rest)): usa el next_hop del path; propaga `rest` al siguiente frame.
  - Err (decode falla): fallback a routing table + `Vec::new()`. Loguea `debug` para diagnóstico.

Logging `debug` en ambas ramas (multipath vs fallback) para trazabilidad.

### `qkc/src/relay.rs::handle_incoming_inner`

Mismo patrón para frames intermedios. Cualquier QKC que recibe un frame `FRAME_RECV`/`FRAME_RELAY` con `header_qkc_mp` no vacío pop-ea el siguiente hop antes de re-enviar.

### Cambios de firma `forward_plaintext` y `send_frame_to_peer`

Añadido parámetro `header_qkc_mp: Vec<u8>` (entre `plaintext`/`ciphertext` y `header_orr_mp`). Se asigna al frame saliente:

```rust
out.header_qkc_mp = header_qkc_mp;  // antes: queda vacío
```

### Limpieza incidental — clippy unused imports en `qkc/src/kme.rs`

Preexistentes que bloqueaban `cargo clippy -p qkc -- -D warnings`:
- `Etsi014KeyIDs` no usado.
- `binary` no usado.

Quitados. R-002 no prohíbe housekeeping de imports unused en `qkc/src/`; lo que protege es wire/binary format + header_qkc_mp semántica + endpoints HTTP. Imports no relacionados son limpieza válida.

## Verificación

- `cargo build -p qkc --release` → OK.
- `cargo test -p qkc --release --lib` → **8 passed** (legacy; no añadí tests aquí — eso es OBJ-008).
- `cargo clippy -p qkc --release --all-targets -- -D warnings` → **verde** por primera vez (era el preexistente roto en iter 014 del agente previo).
- Sin regresiones: sdn 71, orr 75, common 17, wire 21.

## Backwards-compatibility verificada por inspección

- QKC con código nuevo + frame con `header_qkc_mp` vacío (ORR viejo o `MULTIPATH_ENABLED=false`) → `Vec::new()` rama → routing table normal. **Idéntico** a comportamiento `pablopio/qkc:vX` actual.
- QKC con código nuevo + frame con `header_qkc_mp` corrupto → decode falla → fallback routing table. **No crash**. Loguea `debug`.

## Próximo

OBJ-008: tests unit `qkc::relay::tests` (o nuevo `qkc/tests/multipath_relay.rs`):
1. frame con path `[2,3,4]` en QKC-1 → next_hop=2, frame saliente con path `[3,4]`.
2. frame con path `[4]` en penúltimo QKC → next_hop=4, frame saliente con path vacío.
3. frame con `header_qkc_mp` vacío → fallback routing table.
4. frame con `header_qkc_mp` corrupto → fallback routing table sin crash.

## Bloqueos

Ninguno.
