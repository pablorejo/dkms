# Iteración 006 — RPC `GetPathsWithRatios` (Fase B: OBJ-010, cierra Fase B)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-010.

## Qué se hizo

### `proto/sdn.proto`

Nuevo RPC en `SdnControl`:

```protobuf
rpc GetPathsWithRatios (GetPathsWithRatiosRequest)
  returns (GetPathsWithRatiosResponse);
```

Mensajes:

```protobuf
message GetPathsWithRatiosRequest {
  string src_dkms = 1;
  string dst_dkms = 2;
}

message PathWithRatio {
  repeated string qkc_hops = 1;
  double omega = 2;
  double keys_per_second = 3;
}

message GetPathsWithRatiosResponse {
  repeated PathWithRatio paths = 1;
  double total_keys_per_second = 2;
}
```

### `sdn/src/grpc_server.rs`

Handler `get_paths_with_ratios`:

1. Valida `src_dkms` y `dst_dkms` no vacíos.
2. Si `src == dst`: devuelve lista vacía (no error).
3. Resuelve `src_qkc` y `dst_qkc` via `topo.qkc_of_dkms()`.
4. Si los QKCs coinciden: devuelve lista vacía (mismo nodo físico).
5. Recalcula los paths con `k_shortest_paths` + `filter_overlapping_paths` (mismas funciones que `build_commodities` para garantizar consistencia de orden).
6. Lee `mcf_snapshot` actual y, por cada path con `rates_per_path[p_idx] > 0`, emite una `PathWithRatio` con `omega = r_p / total`.

### `sdn/src/service.rs`

Cambio mínimo: `pub(crate) mod tests` (era `mod tests`) y `pub fn make_service_for_test()` envuelve `make_service()` para uso desde `grpc_server::tests`.

### Tests añadidos (4 en `grpc_server::tests`)

1. `get_paths_with_ratios_returns_paths_after_recompute` — flujo happy path; verifica `paths` no vacío, Σ omega ≈ 1.0, Σ kps ≈ total_kps.
2. `get_paths_with_ratios_empty_on_same_src_dst` — `src == dst` → respuesta vacía sin error.
3. `get_paths_with_ratios_rejects_empty_args` → `InvalidArgument`.
4. `get_paths_with_ratios_404_on_unknown_dkms` → `NotFound`.

## Verificación

- `cargo build --workspace --release` → OK (1m 02s primera vez, compila proto en todo el workspace).
- `cargo test -p sdn --lib --release` → **71 passed** (era 67 + 4 nuevos RPC).
- `cargo clippy -p sdn --release --all-targets -- -D warnings` → OK.

### Test preexistente roto (NO causado por este cambio)

`dkms::state::buffer::tests::try_push_returns_key_when_full` falla con `panicked at dkms/src/state/buffer.rs:153:47: full: ()`. Verificado:
- Mis diffs **NO tocan** `dkms/src/` (R-003 lo prohíbe).
- `git log dkms/src/state/buffer.rs` muestra último cambio en commit `6d6264a` (anterior al scaffolding del agente).
- El test existe y se rompe sin que yo haya tocado nada del crate dkms.

Documentado en `.memory.md`. **No bloquea OBJ-010** porque la regla
de oro del agente es "verde el crate tocado" — `cargo test -p sdn`
está verde. Anoto el bug como una incidencia operativa de futura
investigación si el usuario lo prioriza.

## Decisiones

- **Recompute paths en el handler** (no leer `Commodity.paths` cacheada del SdnService). El cliente puede llamar al RPC cuando no haya un snapshot reciente; mejor calcular paths localmente (Yen's + filter) que asumir un cache válido. La consistencia se mantiene porque `k_shortest_paths` es determinístico y `filter_overlapping_paths` con threshold fijo también.
- **`rates_per_path` indexado por orden Yen's**: el handler usa el mismo `k_shortest_paths(...).filter_overlapping_paths(...)` que `build_commodities` para garantizar que `rates_per_path[p_idx]` sea el path `paths[p_idx]`.
- **Lista vacía en lugar de error en `src==dst` o `same QKC`**: el caller decide qué hacer (típicamente: enviar localmente sin red).
- **NOT FOUND si el path QKC no existe en topología**: situación anómala que merece error explícito.

## Hallazgos

- El test `try_push_returns_key_when_full` del crate `dkms` ya estaba roto antes de tocar nada. **NO es mi responsabilidad bajo R-003** (no tocar `dkms/`). Anotado para futura investigación.
- El cambio en `service.rs` para hacer `pub(crate) mod tests` requirió añadir `pub fn make_service_for_test()` porque el módulo entero queda `pub(crate)` pero las funciones internas siguen siendo `fn` (privadas). El helper público envuelve la factory original.
- `tonic-build` regenera correctamente los stubs del proto. Compilación inicial del workspace tarda ~1 minuto la primera vez por la recompilación encadenada.

## Cierre de Fase B

Con OBJ-010 completo, la **Fase B está cerrada** (OBJ-006..OBJ-010, 5/5). El SDN ya tiene:

- Filter de paths con overlap (OBJ-006).
- Solver K-Splittable MCF (OBJ-007).
- Subtract usage multi-path (OBJ-008).
- Forwarding output con omegas (OBJ-009).
- RPC `GetPathsWithRatios` publicado al cliente (OBJ-010).

**Lo que sigue:** Fase C — integración en ORR cliente (OBJ-011..OBJ-013).

## Bloqueos

Ninguno (el test roto de dkms es pre-existente y no bloquea Fase B).
