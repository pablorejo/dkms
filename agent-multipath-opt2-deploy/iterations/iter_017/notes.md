# iter_017 — wait-state OBJ-016 (sim random n=20 d=3 corriendo a 2m27s)

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197`.
**Objetivo activo:** OBJ-016 sigue en marcha (no terminada).

## Estado actual

```
PID 2620945 ELAPSED=02:27 STAT=Sl
sim_id=52 ns=52 Active 2m17s
0/380 commodities saturated
saturation_timeout 600s → restante ~7-8 min hasta cierre
```

## Evidencia multipath en vivo (dkms-764 log)

Muestra de los 19 peers de **dkms-764** a t≈0:21:41Z (sim arrancó ~0:18Z, ~3.5 min de emit):

| peer     | obs kps | sdn kps |
|----------|---------|---------|
| dkms-767 | 16.4    | 20.8    |
| dkms-770 | 47.2    | 47.6    |
| dkms-773 | 47.2    | 48.3    |
| dkms-776 | 29.4    | 32.7    |
| dkms-779 | 49.6    | 52.4    |
| dkms-782 | 27.4    | 32.7    |
| dkms-785 | 26.6    | 29.8    |
| dkms-788 | 26.2    | 32.7    |
| dkms-791 | 34.6    | 42.5    |
| dkms-794 | 43.0    | 46.1    |
| dkms-797 | 17.2    | 20.1    |
| dkms-800 | 51.4    | 51.1    |
| dkms-803 | 101.6   | 141.0   |
| dkms-806 | 193.2   | 192.6   |
| dkms-809 | 26.8    | 47.6    |
| dkms-812 | 57.4    | 55.7    |
| dkms-815 | 39.2    | 54.7    |
| dkms-818 | 18.2    | 20.8    |
| dkms-821 | 171.2   | 173.1   |

- **SDN spread**: min=20.1, median=47.6, max=192.6, sum=1142 kps (sólo dkms-764 origen, 19 peers).
- **SDN coeff of variation local**: 0.840 (subirá/bajará con más muestras).
- 7 peers >50 kps, 12 peers en rango 20-50 kps. Distribución compatible con multipath repartiendo según número de paths disponibles.
- Rate alto en dkms-803, dkms-806, dkms-821: probablemente peers con paths cortos directos. Rate bajo en peers con paths más indirectos.

Para 20 nodos d=3 (random), commodities=380; cada nodo origen tiene 19 peers ⇒ patrón heterogéneo más rico que mesh 3x3 donde todos los pares son simétricos.

## Compared to mesh 3x3 mid-saturation

A los ~3 min mesh 3x3 también mostraba 0/72 saturated (lleva sentido — bootstrap + emit warmup ~2 min). A los 10 min mesh 3x3 era 2/72; a los 11-12 min subió a 6/72. Random n=20 d=3 con 380 commodities probablemente seguirá curva similar: 0-2/380 a los 5 min, ~10-30 a los 10 min, ~30-60 a timeout.

## Decisión iter_017

- **NO interrumpir** sim 52.
- **NO lanzar OBJ-017** en paralelo. Esperar OBJ-016 cierre primero.
- **NO bench todavía** — no hay CSVs.
- Próximo cron (~00:25Z) verificará progreso. Si timeout cumplido + CSVs presentes → cerrar OBJ-016 y arrancar OBJ-017.

## Bloqueos

Ninguno. Espera natural ~10-15 min más.
