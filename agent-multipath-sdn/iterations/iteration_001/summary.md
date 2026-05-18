# Iteración 001 — Documento de diseño completo (Fase A: OBJ-001, OBJ-002, OBJ-003)

**Fecha:** 2026-05-18
**Objetivos trabajados:** OBJ-001, OBJ-002, OBJ-003.

## Qué se hizo

Creado `memory/project_multipath_design.md` (380 líneas) con las 14
secciones requeridas en OBJ-001:

1. Problema (cuello topológico en single-path).
2. Fundamento teórico (water-filling óptimo para max-min, Bertsekas-Gallager).
3. Estado actual del código (tabla con líneas exactas — 375/441/537 en `mcf.rs`).
4. **Decisión sub-prioridades** (OBJ-002) — *pesos status quo*, con razonamiento.
5. Algoritmo K-Splittable MCF (estructura general + water-filling sobre `(commodity, path)`).
6. **Ejemplo numérico de 3 nodos** (OBJ-003) — triángulo A-B-C con caps=10.
   Caso 1: 1 commodity → multipath duplica throughput (10 → 20 keys/s).
   Caso 2: 2 commodities → mismo throughput, mejor balance de carga.
7. 3 cambios concretos en `mcf.rs` con líneas exactas.
8. Nuevo RPC `GetPathsWithRatios` (proto + server + service).
9. Coste estimado: ~830 líneas, ~2.7 sesiones.
10. Beneficio esperado cuantificado (spread ≤ 0.25, mínimo ≥ 0.40, etc.).
11. Trade-offs (max-min vs max-throughput como decisión consciente).
12. **6 criterios de aceptación** con definiciones operacionales resueltas.
13. Validación pre-implementación.
14. Referencias bibliográficas.

## Decisiones tomadas

- **Sub-prioridades = pesos status quo (status quo).** Razonamiento:
  HIGH/LOW ya da prioridad estricta entre tiers; pesos
  decade-spaced (10000/1000/100/10/1) dan suavidad intra-tier; N
  fases oscilarían más con la histéresis existente; coste de
  refactor cero vs medio.
- **Definiciones operacionales de criterios:** spread y mínimo se
  miden por commodity (línea peor de `sat_enc_over_time`); starvation
  = "algún buffer del DKMS bajo 0.15"; producción total = `Σ
  emit_total`; cuello obvio = builder `bridge` (a implementar en
  OBJ-004).

## Verificación

- El documento está en `/home/pablopio/Documentos/trabajo_atlantic/dkms_rust/memory/project_multipath_design.md`.
- No se tocó código Rust en esta iteración, por lo que no aplica
  `cargo build/test/clippy`.
- Próxima iteración debería ejecutar `cargo check` después del
  primer cambio de código (esperado OBJ-004 o OBJ-005, ambos en
  `tests/cli/`, tampoco son Rust).

## Hallazgos

- Las líneas exactas de `paths.first()` en `mcf.rs` son **375, 441,
  537** (+ 708 en `#[cfg(test)] mod tests`, que no se toca). Las 3
  primeras son las que hay que mutar.
- El campo `omega: f64` en `McfSnapshot.forwarding[i]` YA existe,
  documentado como "*kept multi-path-ready*". Es de hecho la
  estructura que el cambio activa.
- El frame de QKC tiene `header_qkc_mp: Vec<u8>` reservado y vacío,
  pero el diseño NO lo usa (el path va en `app_header["orr_path"]`
  del ORR, mecanismo ya verificado).
- Existe `agent-dkms-topo-cli/` (otro agente, no este). Está
  protegido por R-001/restricciones.

## Próximos pasos

OBJ-004 (builder `bridge`) y OBJ-005 (`bench_multipath.py`) en Fase A.
Después arranca Fase B (OBJ-006: `filter_overlapping_paths`).
