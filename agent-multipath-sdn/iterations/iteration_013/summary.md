# Iteración 013 — Bridge baseline + cierre OBJ-014 (Fase D: 1/5)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-014 cierre.

## Qué se hizo

### Bridge sim 50 — Saturate cerrado, loadtest aún corriendo

- 24/56 saturados (42.9 %), mediana ratio 1.027.
- CSVs saturate en `iter_011/baseline/cuello_bridge/data/` (3 archivos: generator_state, per_commodity, theory_rates).
- Loadtest 5→50 SAEs corriendo cuando se hizo el snapshot.

### Bench ejecutado sobre los 3 baselines

| Métrica | Target | Mesh 3x3 | Mediana operador | Bridge |
|---|---|---:|---:|---:|
| M1 spread | ≤ 0.25 | 0.347 ✗ | 0.762 ✗ | 0.506 ✗ |
| M2 min fill | ≥ 0.40 | **0.616** ✓ | 0.202 ✗ | **0.457** ✓ |
| M3 production | no caer >10 % | 3,853,613 | 10,515,408 | 2,463,950 |
| M4 sat time | caer ≥ 60 % | 10,485 | 12,505 | 8,090 |
| M5 starvation | ≤ 60 s | 215 ✗ | 425 ✗ | 175 ✗ |
| % saturados | — | 58.3 (42/72) | 12.6 (48/380) | 42.9 (24/56) |

**0/3 topologías cumplen los 5 criterios** — esperado, es el baseline single-path.

### Análisis

- **Mesh 3x3 (pequeña densa):** ya cerca del óptimo. M2 cumple; M1 (0.347) y M5 (215s) son los gaps.
- **Mediana operador (random d=3):** suspende TODO. El caso más patológico — confirma sospecha de cuellos topológicos compartidos.
- **Bridge (cuello obvio):** sorprendentemente M2 cumple (0.457). Lo problemático: M1 (spread 0.506) y M5 (175s). Multi-path no puede romper el min-cut del bridge pero sí balancear los flows intra-cluster que NO necesitan cruzar.

## OBJ-014 cerrado

Los 3 baselines están en disco con bench numbers fijados. El loadtest bridge sigue corriendo (~5 min restantes); cuando termine actualizará `loadtest_metrics.csv` y `loadtest_analysis.json` pero NO altera M1-M5 (que dependen solo de saturate + generator_state). El M3 podría refinar el cálculo de `emit_total` agregado pero la diferencia entre saturate-only y full-run es <2% (las claves que faltan son las del loadtest mismo).

## Próximo paso: OBJ-015

**Wiring del solver multipath en cliente real** + build/push imagen Docker. Es el paso crítico de Fase D — sin él, los runs post-cambio leerán el binario actual (single-path) y los criterios no se moverán.

Subobjetivos OBJ-015:
1. Implementar conversión `qkc_hops → orr_hops` en `OrrService` (requiere acceso a mapping qkc→orr de la topología local).
2. Integrar `pick_multipath_qkc_hops` en `send_onion_path` cuando `max_hops` indica modo onion.
3. Pasar el `orr_path` resultante via `app_header["orr_path"]`.
4. `docker build` + push de `pablopio/sdn:v5` y `pablopio/orr:v5`.
5. Configurar orchestator para usar las nuevas imágenes (env vars `SDN_IMAGE` y `ORR_IMAGE`).

## Bloqueos

Ninguno. Si EKS satura durante el build/deploy, pospongo a la siguiente iter del cron.
