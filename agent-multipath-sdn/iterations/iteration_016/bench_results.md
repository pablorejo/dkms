# Bench Results — Fase D (iter 016)

**⚠️ CAVEAT IMPORTANTE:** El "post-cambio" de esta iteración es **idéntico al baseline**. Razón: el código multi-path (Fase A-C) está en working tree y unit-tested, pero **no se ha desplegado en imagen Docker** ni se ha **wired en `send_onion_path`**. El cluster EKS ejecuta `pablopio/sdn:v7` / `orr:v2` (single-path).

Para obtener un post real distinto del baseline se requiere OBJ-021 (wiring + docker push), pendiente de la decisión del usuario.

Esta tabla documenta el estado actual con honestidad. Cualquier "cambio" reportado debajo es 0.0 por construcción.

## Resumen criterios

| Topología | Criterios cumplidos (baseline) | Criterios cumplidos (post == baseline) |
|---|---|---|
| Pequeña densa (mesh 3x3) | 2/5 | 2/5 |
| Mediana operador (random n=20 d=3) | 1/5 | 1/5 |
| Cuello obvio (bridge 4×2) | 2/5 | 2/5 |

**Cumplimiento del criterio "≥2 de 3 topologías cumplen los 5"**: NO se cumple ni esperaría cumplirse — los 6 criterios no se mueven cuando no hay cambio.

## Detalle por topología

### Pequeña densa (mesh 3x3)

| Métrica | Baseline | Post | Cambio | Umbral | Pasa |
|---|---:|---:|---:|---|---|
| M1 spread | 0.347 | 0.347 | 0.00 % | ≤ 0.25 | ✗ |
| M2 min fill | 0.616 | 0.616 | 0.00 % | ≥ 0.40 | ✓ |
| M3 production | 3,853,613 | 3,853,613 | 0.00 % | ≤ +10% drop | ✓ |
| M4 sat time | 10,485 | 10,485 | 0.00 % | ≥ 60 % drop | ✗ |
| M5 starvation | 215 s | 215 s | 0.00 % | ≤ 60 s | ✗ |

### Mediana operador (random n=20 d=3)

| Métrica | Baseline | Post | Cambio | Umbral | Pasa |
|---|---:|---:|---:|---|---|
| M1 spread | 0.762 | 0.762 | 0.00 % | ≤ 0.25 | ✗ |
| M2 min fill | 0.202 | 0.202 | 0.00 % | ≥ 0.40 | ✗ |
| M3 production | 10,515,408 | 10,515,408 | 0.00 % | ≤ +10% drop | ✓ |
| M4 sat time | 12,505 | 12,505 | 0.00 % | ≥ 60 % drop | ✗ |
| M5 starvation | 425 s | 425 s | 0.00 % | ≤ 60 s | ✗ |

### Cuello obvio (bridge 4×2)

| Métrica | Baseline | Post | Cambio | Umbral | Pasa |
|---|---:|---:|---:|---|---|
| M1 spread | 0.506 | 0.506 | 0.00 % | ≤ 0.25 | ✗ |
| M2 min fill | 0.457 | 0.457 | 0.00 % | ≥ 0.40 | ✓ |
| M3 production | 2,463,950 | 2,463,950 | 0.00 % | ≤ +10% drop | ✓ |
| M4 sat time | 8,090 | 8,090 | 0.00 % | ≥ 60 % drop | ✗ |
| M5 starvation | 175 s | 175 s | 0.00 % | ≤ 60 s | ✗ |

## Gráficas

Generadas con `bench_multipath.py --plot-compare`:
- `plots/sat_enc_over_time_compare_pequena_densa.png` (247 KB)
- `plots/sat_enc_over_time_compare_mediana_operador.png` (495 KB)
- `plots/sat_enc_over_time_compare_cuello_bridge.png` (172 KB)

Los dos paneles de cada gráfica son **idénticos**, evidenciando que post == baseline.

## Hipótesis de mejora con wiring real (OBJ-021)

Si se completara OBJ-021 (wiring + docker push), basándose en el ejemplo numérico del documento de diseño y en la hipótesis "multipath SÍ aporta en cuellos topológicos":

- **Mediana operador (random d=3)**: M1 debería bajar de 0.76 a < 0.30; M5 de 425s a < 100s; % saturados de 12.6% a > 30%.
- **Bridge**: M1 de 0.51 a < 0.30 (multipath balancea intra-cluster); M5 de 175s a < 100s; M2 mantiene cumplido.
- **Mesh densa**: efecto pequeño (ya está cerca del óptimo); M1 baja de 0.35 a ~0.20–0.25.

Estos números son **predicciones**, no medidos. Verificables cuando OBJ-021 esté hecho.
