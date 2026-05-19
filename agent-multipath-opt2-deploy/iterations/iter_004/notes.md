# iter_004 — OBJ-005 (tests integración wiring ORR)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-005.

## Qué se hizo

6 tests nuevos en `orr::service::multipath_cache_tests` cubriendo los puntos críticos del helper `compute_qkc_path_header`:

1. **`parse_qkc_hops_and_encode_roundtrip`** — happy path: `Vec<String>` numéricos parsea a `Vec<u32>` correctamente; encode + decode preserva los ids. Verifica el flujo completo de la rama exitosa del helper.
2. **`parse_qkc_hops_fails_on_non_numeric`** — fallback: si algún String no parsea (e.g. `"abc"`), `Result::Err` propaga. El helper convierte esto a `Vec::new()` → fallback single-path.
3. **`parse_qkc_hops_handles_large_ids`** — qkc_ids reales del cluster en rango `100_000 + node_id` (cota R-008 node_id≤155 → ids `100001..100155`). Encode + decode preserva valores grandes.
4. **`env_var_recognized_values`** — exactamente `"true"` (case-insensitive) cuenta como ON; `"1"`, `"yes"`, `""`, `"false"` → OFF. Simula la lógica de `std::env::var("MULTIPATH_ENABLED").map(|v| v.eq_ignore_ascii_case("true")).unwrap_or(false)`.
5. **`app_header_missing_keys_means_no_multipath`** — si `app_header["src_dkms"]` o `["dst_dkms"]` ausente, helper sale por la guarda y retorna `Vec::new()`.
6. **`sampler_to_wire_roundtrip_e2e`** — flujo completo simulado SIN OrrService: sampler.sample → idx → parse Strings → encode + decode. Verifica que el path muestreado del cache es uno de los disponibles y roundtrip preserva.

## Decisión: tests sin OrrService completo

Construir un `OrrService` real para tests asíncronos requiere identity ML-KEM + QkcLink + SDN client mock. El agente previo ya documentó esto como costoso. Los tests añadidos cubren la **lógica nueva del helper** (parse, env, app_header guards, sampler→wire flow) usando primitivas testeable sin OrrService. La integración real (helper async invocando pick_multipath_qkc_hops sobre OrrService) se valida con el smoke test local de OBJ-011.

## Issues encontrados y arreglados

- **`Result<Vec<u32>, _>` alias colision**: el crate define `type Result<T> = std::result::Result<T, OrrError>`; los tests deben usar `std::result::Result<Vec<u32>, _>` explícito para evitar mismatch con `ParseIntError`.
- **Clippy `useless_vec`**: `vec![...]` con datos estáticos en tests → cambio a `[...]` array (sin alocar).
- **Clippy `unnecessary_get`**: `map.get(k).is_none()` → `!map.contains_key(k)`.

## Verificación

- `cargo test -p orr --release --lib multipath_cache_tests` → **11 passed** (era 5 + 6 nuevos).
- `cargo test -p orr --release --lib` → **75 passed** (era 69 + 6 nuevos).
- `cargo clippy -p orr --release --all-targets -- -D warnings` → verde.

## Próximo

**Fase B completa (3/3)**. Próximo: OBJ-006 — QKC consume el path en `qkc/src/relay.rs::handle_local_send`. Fase C.

## Bloqueos

Ninguno.
