# Iteración 003 — `bench_multipath.py` (Fase A: OBJ-005 — completa Fase A)

**Fecha:** 2026-05-18
**Objetivo trabajado:** OBJ-005.

## Qué se hizo

1. **`tests/cli/bench_multipath.py`** (~380 líneas): script CLI que lee
   `data/per_commodity.csv` y `data/generator_state.csv` de un run y
   emite las **5 métricas de aceptación** medibles offline.

   La 6ª métrica del documento de diseño (coste solver < 200 ms,
   memoria, binario) se mide en Fase D con `cargo bench`, NO aquí.

   Funciones clave:
   - `compute_metrics(run_dir, ...)` → `RunMetrics` con M1-M5.
   - `verify_against_baseline(post, baseline)` → 5 `CriterionResult` con
     `passed: bool`.
   - CLI con `--baseline` para comparar; exit code 0 si pasa, 1 si no.
   - `--json` para salida estructurada.

2. **`tests/cli/test_bench_multipath.py`** (~200 líneas, 5 tests):
   - 2 commodities mínimo (uno saturado, uno parcial) → métricas
     correctas.
   - Starvation continua detectada en buffer eternamente bajo umbral.
   - `verify_against_baseline` produce 5 criterios bien-formados.
   - `FileNotFoundError` si faltan los CSVs.
   - El `warmup` excluye eventos tempranos del cómputo.

## Métricas del baseline (`random n=20 d=3` sim 48)

| Métrica | Valor | Target multipath |
|---|---:|---|
| **M1 Spread fill ratio** | **0.7619** | ≤ 0.25 (falla factor 3×) |
| **M2 Min fill ratio** | **0.2023** | ≥ 0.40 (falla factor 2×) |
| **M3 Total production keys** | **10,515,408** | no caer > 10 % |
| **M4 Saturation time (s)** | **12,505** | caer ≥ 60 % → ≤ 5,002 |
| **M5 Max starvation continua (s)** | **425** | ≤ 60 (falla factor 7×) |
| Median fill ratio | 0.3366 | (contextual) |
| % saturados | 12.6 (48/380) | (contextual) |

**Conclusión:** el baseline NO cumple los criterios — esperado, es
single-path. El cambio K-Splittable MCF de Fase B-D debe llevar todos
estos números a los umbrales.

## Verificación

- `pytest tests/cli/test_bench_multipath.py -v` → **5 passed**.
- `pytest tests/cli/ -q` → **190 passed** (era 185 + 5 nuevos).
- Smoke CLI baseline: salida text + JSON funcionan.
- Sin cambios Rust en esta iter.

## Decisiones

- **Bucket de 5s para starvation continua.** Coincide con la frecuencia
  de `generator.state` (cada 5 s). Más fino sería ruido; más grueso
  perdería transitorios.
- **Definición "DKMS starved" = alguno de sus buffers < 0.15.**
  Acordado en sesión de diseño (sec 12 del documento).
- **Warmup default 30 s.** El SDN, ORR bootstrap y primera asignación
  de rate llegan en ese plazo. Antes de eso, todo es transitorio
  inicial.
- **NO se mide criterio 6 aquí.** Coste de solver, memoria y binario
  son métricas de Fase D (`cargo bench`).
- **Exit code 0 si TODOS los criterios pasan**, 1 si alguno falla.
  Pensado para integración CI/regresión.

## Hallazgos del baseline

- **M5 (starvation) = 425 s es el más dramático**. Algún DKMS pasa
  ~7 min con al menos uno de sus buffers atorado < 15 %. Multi-path
  debería romper estos cuellos prolongados.
- **M3 (producción total) = 10.5 M keys**. Una referencia útil:
  multi-path no debería caer por debajo de 9.5 M (90 %).
- La métrica `median_fill_ratio` (0.337) confirma que en régimen
  estacionario los buffers están desbalanceados; un valor sano sería
  0.5-0.7.

## Próximos pasos

**Fase A completa (5/5 objetivos).** Próximo: **OBJ-006**, primer
cambio Rust — `filter_overlapping_paths` en `sdn/src/mcf.rs`.

## Bloqueos

Ninguno.
