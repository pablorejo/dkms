# iter_002 — OBJ-002 (helpers msgpack qkc_path en wire/)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-002.

## Qué se hizo

Nuevo módulo `wire/src/qkc_path.rs` (~155 líneas) + reexport en `wire/src/lib.rs`:

- `pub fn encode_qkc_path(path: &[u32]) -> Vec<u8>`.
- `pub fn decode_qkc_path(bytes: &[u8]) -> Result<Vec<u32>, QkcPathError>`.
- `pub fn pop_qkc_path_next_hop(bytes: &[u8]) -> Result<(u32, Vec<u8>), QkcPathError>`.
- `pub enum QkcPathError { Encode, Decode, EmptyPath }` con `thiserror`.

**Decisiones (consistentes con sección 3 del doc de diseño):**
- Path vacío → `Vec::new()` (no msgpack-with-empty-array). Distingue "sin source routing" de "0 hops".
- `decode_qkc_path(&[]) → Ok(vec![])`: backwards-compat con QKC viejo / paquetes sin path.
- `decode_qkc_path(garbage) → Err`: el QKC caller decide fallback (typically routing table).
- `pop_qkc_path_next_hop(empty)` → `Err(EmptyPath)`: el QKC sabe que el path se agotó.

Struct interno `QkcPathFrame { #[serde(rename="qkc_path")] qkc_path: Vec<u32> }` para que `rmp_serde::to_vec` produzca el map con la key literal `qkc_path` sin pasar por `BTreeMap` (más overhead).

## Tests (11 nuevos)

1. `encode_empty_path_returns_empty_bytes` — convención path vacío.
2. `decode_empty_bytes_returns_empty_path` — backwards-compat.
3. `roundtrip_single_hop` — `[42]`.
4. `roundtrip_multi_hop` — `[1,2,3,7,100,65535]`.
5. `pop_single_hop_returns_empty_rest` — pop deja Vec::new() bytes.
6. `pop_multi_hop_returns_rest` — pop devuelve next_hop=2 + rest=[3,4].
7. `pop_chained_consumes_all_hops` — pop hasta EmptyPath, consume todos.
8. `pop_empty_bytes_returns_empty_path_error` — pop sobre vacío → EmptyPath.
9. `decode_garbage_returns_err` — bytes random no-msgpack.
10. `decode_msgpack_with_wrong_schema_returns_err` — msgpack válido (map vacío) sin key qkc_path.
11. `bytes_size_reasonable_for_typical_path` — 5 hops ≤ 30 bytes (verifica overhead aceptable).

## Verificación

- `cargo build -p wire --release` → OK (tras quitar import unused `BTreeMap`).
- `cargo test -p wire --release --lib` → **21 passed** (era 10 + 11 nuevos).
- `cargo test -p wire --release --lib qkc_path` → 11/11 passed.
- `cargo clippy -p wire --release --all-targets -- -D warnings` → verde.
- `cargo test -p sdn -p orr -p qkc -p common --release --lib` → 71+57+0+17 = todos verde, no regresiones.

## Próximo

OBJ-003: cablear `pick_multipath_qkc_hops` en `orr/src/service.rs::send_message` con:
- env `MULTIPATH_ENABLED=true` opt-in (R-015).
- Parse `Vec<String> → Vec<u32>` con fallback a single-path si falla.
- `frame.header_qkc_mp = wire::encode_qkc_path(&parsed)`.
- Logging tracing::debug.

## Bloqueos

Ninguno.
