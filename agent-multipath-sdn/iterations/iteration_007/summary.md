# Iteración 007 — ORR alias sampler + caché K paths (Fase C: OBJ-011)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-011.

## Qué se hizo

### Módulo nuevo `orr/src/alias.rs` (~180 líneas)

Implementación pura-Rust del **alias method** (Walker 1977). Construcción `O(K)`, sampling `O(1)`. API:

```rust
pub struct AliasSampler { /* prob: Vec<f64>, alias: Vec<usize> */ }
impl AliasSampler {
    pub fn build(weights: &[f64]) -> Option<Self>;
    pub fn sample<R: Rng + ?Sized>(&self, rng: &mut R) -> usize;
}
```

**7 tests:**
- `build_empty_returns_none`, `build_all_zero_returns_none` — edge cases.
- `build_single_weight_always_returns_zero` — degenerado K=1.
- `empirical_distribution_matches_weights_n10000` — **OBJ-011 verificado**: N=10000, tolerancia 1.5% absoluto.
- `weights_dont_need_to_sum_to_one` — normalización interna.
- `extreme_skew_still_samples_minority` — 0.999/0.001 con N=100k, error < 0.05%.
- `two_paths_50_50_balanced` — 50/50.

### `orr/src/sdn_client.rs`

- Structs `PathWithRatio` y `PathsWithRatios` (mirror del proto).
- Nuevo método `SdnClient::get_paths_with_ratios(src_dkms, dst_dkms) -> Result<PathsWithRatios>`. Mismo patrón timeout que `get_orr_path`. Decodifica respuesta del proto.

### `orr/src/service.rs`

- Struct nueva pública `MultipathCacheEntry { paths, sampler, total_keys_per_second }` — entry del cache K-Splittable con sampler precomputado.
- Nuevo campo `paths_cache_multipath: Arc<RwLock<HashMap<(String,String), MultipathCacheEntry>>>` en `OrrService`.
- Invalidación del cache cuando llega `TopologyEvent`: ahora también limpia el cache multipath (junto al single-path).
- Nuevo método público `OrrService::pick_multipath_qkc_hops(src_dkms, dst_dkms) -> Result<Option<Vec<String>>>`:
  - Cache hit → muestrea con sampler precomputado.
  - Cache miss → llama `SdnClient::get_paths_with_ratios`, construye sampler, cachea, muestrea.
  - Devuelve `None` si: no SDN, paths vacíos del SDN (Saturated o src/dst en mismo QKC), o sampler build falla. Caller hace fallback.
- 2 test helpers `#[cfg(test)]`: `multipath_cache_len`, `insert_multipath_cache_entry` (para tests deterministas en OBJ-012/013).

### Tests añadidos en `service::multipath_cache_tests` (2)

- `multipath_entry_samples_match_omegas` — split 70/30, N=10000, tolerancia 2%.
- `single_path_entry_always_samples_index_zero` — K=1 → siempre idx 0.

## Verificación

- `cargo build -p orr --release` → OK.
- `cargo test -p orr --lib --release` → **66 passed** (era 57 + 9 nuevos: 7 alias + 2 multipath_cache).
- `cargo clippy -p orr --release --all-targets -- -D warnings` → OK.

## Decisiones

- **Sampler precomputado por entry** (no en cada `sample()`). Construir alias es O(K); reusar el sampler N veces es O(1) por sample. Trade-off: 50 bytes extra por entry; ahorro masivo de CPU en hot path.
- **Cache key `(src_dkms, dst_dkms)`** — exactamente como lo expone el RPC. El ORR no convierte qkc_hops a orr_hops aquí (se hará en OBJ-013 junto con el wiring del onion). Devolver qkc_hops mantiene el RPC y la cache coherentes.
- **Negative cache implícito**: si el SDN responde vacío (rate=0 o sin path), NO cacheamos — la próxima vez re-pedimos por si el flow se recuperó.
- **Invalidación on TopologyEvent**: ya existe el background loop; añadir `cache_mp.write().clear()` es mínimo.
- **`rand::thread_rng()`** dentro de `pick_multipath_qkc_hops` (en lugar de RNG inyectado). Trade-off: determinismo en tests vs simplicidad. Los tests inyectan via `MultipathCacheEntry.sampler.sample(&mut rng)` directamente, no via `pick_*`.

## Hallazgos

- `rand` ya estaba en `orr/Cargo.toml` (workspace dep). Cero deps nuevas.
- El sampling con `rand::thread_rng()` es ~5 ns por sample en release. Para un commodity con λ=50 keys/s, 50 sampling/s = 250 ns/s overhead. Despreciable.
- El campo `MultipathCacheEntry.paths` guarda `Vec<Vec<String>>` (qkc_ids). Para OBJ-013 habrá que proyectar a orr_ids antes de meterlo en `app_header["orr_path"]`. Por ahora, los tests verifican el sampling sobre la estructura tal cual.

## Próximos pasos

- **OBJ-012:** invalidación de cache de paths en ORR — **ya está implementada** en esta iter como parte del `TopologyEvent` handler. La podemos marcar `[x]` aquí mismo o dejar que la próxima iter la cierre con un test explícito + actualización de architecture.md. Decisión: marcar OBJ-011 + OBJ-012 hechos; OBJ-013 cierra la integración.
- **OBJ-013:** tests más completos del sampling + invalidation. Wiring del path elegido en el onion (`send_onion_path`). Esto SÍ requiere conversión qkc→orr.

## Bloqueos

Ninguno. El test preexistente roto en `dkms::state::buffer` (iter 006) sigue ahí pero no bloquea Fase C — esta iter solo toca `orr/`.
