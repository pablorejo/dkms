# Iteración 011 — Bridge baseline lanzada (Fase D: OBJ-014 2/3 en curso)

**Fecha:** 2026-05-18

## Estado

- **mesh 3x3 (sim 49)**: saturate cerrado con **42/72 saturados, mediana ratio 0.949**. CSVs escritos en `iter_009/baseline/pequena_densa_mesh3x3/data/`. Loadtest aún corriendo cuando se hizo este snapshot — termina en ~3 min.
- **bridge (sim 50)** lanzada **paralela** porque el cluster autoescala bien y no quería gastar otra iter cron entera. PID 2313867. Output `iter_011/baseline/cuello_bridge/`. Esperable terminar en ~12-15 min.
- **mediana operador** ya en `iter_010/baseline/mediana_operador/` (iter previa).

## Pendiente

- Iter 012: verificar mesh terminó completo (con loadtest CSVs) + bridge sigue/terminó.
- Iter 013-014: ejecutar `bench_multipath.py` sobre los 3 baselines, fijar números de partida, marcar OBJ-014 [x].
