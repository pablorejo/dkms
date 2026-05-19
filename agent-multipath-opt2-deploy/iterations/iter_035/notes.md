# iter_035 — wait-state OBJ-028 (sim 59 bridge K=2 a 1m08s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-028 (sim 59 bridge).

## Estado

```
PID 3103150 ELAPSED=01:08 STAT=Sl
sim_id=59 ns=59 Active 62s
8 DKMS pods Ready (8 nodos = bridge --cluster-n 4 --cluster-count 2)
0/56 commodities saturated (bootstrap)
saturation_timeout 600s → ETA fin sim ~13 min
```

## Recordatorios y stake

Resultados parciales OBJ-028:
- mesh3x3 sim 57 (K=2): CUMPLE R-017 ✓ (M1 -47%, M3 +143%, M5 -57%).
- random sim 58 (K=2): NO cumple solo por M1 (-14.2% vs -30% req); M3+M5 OK.

R-017 estricto por criterio:
- M1 ≥-30% en ≥2/3: mesh ✓, random ❌, bridge ?
- M3 no caer >10% en 3/3: mesh ✓, random ✓ (+10% borderline), bridge ?
- M5 ≥-50% en ≥2/3: mesh ✓, random ✓ → 2/3 ya OK.

**Bridge necesita cumplir M1 ≥-30% para que R-017 global cumpla** (vía M1 2/3 con mesh+bridge). En sim 53 (bridge K=3+0.70) M1 era +6.8% (algoritmo degenera a single-path).

Predicción bridge K=2: similar al sim 53 porque el cuello inter-cluster no tiene paths alternativos. M1 probable ~0-5% → NO cumple.

## Decisión

- **NO interrumpir** sim 59. ETA ~13 min.
- Próximo cron coge sim 59 más avanzada.

## Plan post-bridge

Si bridge K=2 NO cumple M1 → OBJ-028 fallido → arrancar OBJ-029 con **cap por commodity** (`MAX_RATE_PER_COMMODITY_KPS`).
- Hipótesis cap: limitar max_rate por commodity → previene concentración → M1 cae.
- Riesgo: si el cap es demasiado bajo, M3 cae.

Si bridge K=2 SÍ cumple M1 → 2/3 R-017 cumple → continuar a Fase G (cierre + commit).

## Bloqueos

Ninguno. Espera natural.
