# iter_001 — OBJ-001 (documento de diseño wiring opción 2)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-001.

## Qué se hizo

Creado `memory/project_multipath_opt2_wiring.md` (262 líneas, 11 secciones):

1. Problema que resuelve (gap entre Fases A/B/C y wiring real).
2. Por qué opción 2 vs descartadas A (per-flow table), B (MPLS), C (max_hops=-1).
3. Formato exacto msgpack `{"qkc_path": [u32, ...]}` + bytes esperados + convención path-vacío.
4. Diagrama end-to-end paso a paso (7 pasos, topología ejemplo).
5. Tabla hand-off por componente (qué cambia / qué no).
6. Feature flag `MULTIPATH_ENABLED` opt-in (R-015) + rollback trivial.
7. Backwards-compatibility: las 4 combinaciones viejo/nuevo ORR/QKC.
8. Lo que NO se toca (R-002, R-003, ETSI, onion, forwarding fallback, topology).
9. Riesgos identificados con mitigaciones.
10. Criterios de éxito (R-017).
11. Referencias cruzadas.

**Decisión documentada:** parsing `String → u32` en el ORR antes de encodear, con fallback a single-path si falla. Justificado en sección 3.

**Decisión documentada:** path vacío = `header_qkc_mp` vacío (Vec::new()), NO msgpack con array vacío. Distingue "no source routing" de "0 hops". Justificado en sección 3.

## Verificación

- Documento existe en `/home/pablopio/Documentos/trabajo_atlantic/dkms_rust/memory/project_multipath_opt2_wiring.md`.
- No se tocó código Rust → no aplica `cargo build/test/clippy`.

## Próximo

OBJ-002: implementar helpers `encode_qkc_path` / `decode_qkc_path` / `pop_qkc_path_next_hop` en `wire/`. Decidir si va en `wire/src/lib.rs` directamente o crear `wire/src/qkc_path.rs` reexportado. Tests roundtrip + edge cases (vacío, corrupto, etc.).

## Bloqueos

Ninguno.
