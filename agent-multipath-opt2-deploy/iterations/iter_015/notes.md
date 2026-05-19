# iter_015 — sim mesh 3x3 a 10m16s, 2/72 saturated, comparativa contra baseline

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-015 (sim mesh 3x3 sigue corriendo, NO terminada).

## Estado actual

```
PID 2597119 ELAPSED=10:16 STAT=Sl
sim_id=51 ns=51 Active 10m
saturation 2/72 commodities (progresó de 0/72 en iter_014)
saturation_timeout = 600s → próxima fase = SAE test ~3-5 min
```

## Baseline iter_009 — referencia recuperada

Localizado en `agent-multipath-sdn/iterations/iteration_009/baseline/pequena_densa_mesh3x3/sat_analysis.json`.

**Resultados baseline (single-path):**
- total commodities: 72
- saturated: **42 / 72** (58 %)
- median_ratio observed/theory: **0.949** (95 % del rate teórico cumplido)
- ejemplo `dkms-683→dkms-686`: theory=79.4 kps, observed=78.6 kps, sin saturar (only emit_total=40740, ~62 % cap), por límite de tiempo no de rate.

## Comparativa post (sim 51, vivo)

- Saturated a 10m16s: **2 / 72** (3 %)
- Rates observados en `generator.state` de dkms-736:
  - 5 peers ~155-170 kps (rate alto)
  - 3 peers ~30-36 kps (rate bajo) — peers dkms-745, dkms-754, dkms-757

## Análisis: ¿regresión o efecto esperado de multipath?

El criterio de saturación (`enc ≥ 0.95 * 65536`) **no es buen proxy para evaluar multipath**.
Razón: multipath reparte tráfico entre K caminos paralelos. Para un commodity dado:

- **Single-path baseline**: 1 path, rate completo SDN → ~79 kps → llena buffer en ~825 s teóricos.
- **Multipath post**: K paths, rate completo SDN se REPARTE entre K → cada path va más lento individual, pero la suma logra **mayor throughput agregado** porque varios caminos en paralelo consumen capacidad de edges distintos.

**Lo que importa medir** es M1, M3, M5 sobre los CSVs finales, NO el conteo de saturated.

- **M1 spread (varianza relativa rate per-commodity)**: baseline tenía 0.949 median ratio (estrecho). Si post tiene mayor spread → mal. Si post tiene MENOR spread (más equitativo) → bien.
- **M3 throughput total**: sumar `last_sdn_rate_keys_per_s` de todos los commodities. baseline ≈ 42*~80 + 30*~50 = 4860 kps total estimado. Si post está en mismo orden → OK.
- **M5 starvation**: commodities con `last_sdn_rate=0` por mucho tiempo. baseline tiene 30 sin saturar → posible starvation. Post debe ser ≤50 % del baseline.

## Decisión iter_015

- **NO interrumpir** sim 51. Faltan ~30s para timeout 600s → empieza SAE test → finaliza ~5 min más.
- **NO lanzar OBJ-016/017** hasta tener CSVs de OBJ-015. Lanzar 2 sims paralelas viola R-007 mental model.
- **Próximo cron (00:18Z aprox)**: verificar PID muerto y `sat_analysis.json` escrito. Si presente, marcar OBJ-015 [x] y arrancar OBJ-016 (random n=20 d=3).

## Riesgo identificado

El criterio R-017 está formulado en términos de M1/M3/M5 (no de "saturated count"). Pero el script `bench_multipath.py` (OBJ-018) debe poder calcular M1/M3/M5 incluso con conteo de saturados bajo. Verificar en iter_018 que el script no requiere commodities saturados para sus métricas.

## Bloqueos

Ninguno. Espera natural ~5-7 min para que la sim termine.
