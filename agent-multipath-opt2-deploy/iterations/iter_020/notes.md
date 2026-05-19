# iter_020 — wait-state OBJ-017 (sim 53 bridge a 1m40s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-017 (sim 53 sigue corriendo).

## Estado actual

```
PID 2644077 ELAPSED=01:40 STAT=Sl
sim_id=53 ns=53 Active 95s
0/56 commodities saturated (recién bootstrap, sin emit aún)
saturation_timeout 600s → restante ~8.5 min
```

## Baseline iter_011 bridge — números reales recuperados

| Métrica | Valor | Notas |
|---|---|---|
| total commodities | 56 (8 nodos × 7) | — |
| saturated | 24/56 (42.9 %) | — |
| **M1 SDN CV** | **0.874** | — |
| **M3 SDN sum** | **1587 kps** | bajo, cuello obvio |
| **M5 starvation (rate<1)** | **24/56 = 42.9 %** | **muy alta** |
| median ratio obs/theory | 1.027 | — |

**Esta topología es la candidata más fácil para multipath**: el cuello entre clusters es exactamente el caso de uso. Si bridge tampoco mejora, la migración no es viable.

## Umbrales R-017 para OBJ-017 (post)

Con baseline iter_011 single-path:
- **M1 CV** baseline 0.874 → req ≤ **0.612** (≥-30%)
- **M3 sum** baseline 1587 kps → req ≥ **1428 kps** (no caer >10%)
- **M5 starv** baseline 24/56=42.9% → req ≤ **21.4%** (≥-50%)

## Stake

Estado parcial Fase F:
- mesh 3x3 ✓ cumple R-017 con margen enorme.
- random n=20 d=3 ❌ no cumple (M1 EMPEORÓ).
- bridge — pendiente.

**Bridge tiene que cumplir** para 2/3 mínimo → continuar Fase G. Si no → `BLOQUEADO_CRITERIOS_NO_CUMPLIDOS`.

## Decisión iter_020

- **NO interrumpir**.
- ETA fin sim ~00:44Z (timeout 600s + loadtest 225s).
- Próximo cron coge sim terminada con sat_analysis.json + loadtest_analysis.json.

## Bloqueos

Ninguno. Espera natural ~10 min.
