# iter_014 — esperando sim mesh 3x3 (post-cambio) termine

**Fecha:** 2026-05-19
**Trigger:** cron `cfd78197` cada 5 min.
**Objetivo activo:** OBJ-015 (lanzada en iter_013).

## Estado del proceso

```
PID 2597119 ELAPSED=07:24 STAT=Sl
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 --buffer-saturated --sae-test
   --force --node-id-offset 121 --saturation-timeout 600
   --username config_user --password config_password
   --output-dir agent-multipath-opt2-deploy/iterations/iter_013/post/pequena_densa
```

- sim_id=51, ns=51, 9 dkms pods Ready.
- editor→bd map: node-122..130 → dkms-736..760.
- 72 commodities esperados (9×8 ordered pairs).
- 0/72 saturadas a los 7m24s.

## Análisis preliminar (logs vivos, no terminales)

### DKMS `generator.state` (peer rates heterogéneos — posible signo multipath)

Sample dkms-736 a 00:09:25Z:

| peer     | enc   | dec   | emit_total | observed kps | sdn kps |
|----------|-------|-------|------------|--------------|---------|
| dkms-739 | 32591 | 32846 | 32591      | 155.6        | 158.9   |
| dkms-742 | 32365 | 32395 | 32365      | 150.8        | 153.9   |
| dkms-745 | 36099 | 36213 | 36099      | 36.0         | 30.6    |
| dkms-748 | 32051 | 32013 | 32051      | 161.6        | 170.1   |
| dkms-751 | 32789 | 32814 | 32789      | 161.6        | 170.1   |
| dkms-754 | 33709 | 33620 | 33709      | 35.8         | 30.6    |
| dkms-757 | 33570 | 39499 | 33570      | 35.8         | 30.6    |
| dkms-760 | 32817 | 32336 | 32817      | 161.6        | 170.1   |

`observed_keys_per_s ≈ sdn_rate_keys_per_s` siempre (cumple
contrato token-bucket). El SDN está dando dos buckets de rate:
**~30 kps** (3 peers) y **~155-170 kps** (5 peers). Distribución
bimodal compatible con multipath repartiendo cuando un commodity
toma 2-3 caminos vs cuando toma 1.

### ORR bootstrap

- 8 pares orr-229 ↔ orr-{230..237} bootstrap_secret OK.
- `orr.path_cache invalidated version=1 invalidated_singlepath=0 invalidated_multipath=0` — invalidaciones triviales (caches vacíos).

### QKC keystore activo

- `taken=330070` (claves consumidas a 100123)
- `wenc=330070`, `misses=0` (peer 100123) y `misses=26` (peer 100125)
- Coherente con QKD-pull funcionando, claves fluyendo a través de QKC↔QKC peers.

### ¿Por qué 0/72 a los 7 min?

`enc ≈ 32-36k` contra cap 65536 → **50-55 % llenos**.
`dec ≈ enc` → el peer drena tan rápido como generamos.

Criterio de saturación en `dkms_topo.py`: `enc ≥ 0.95 × capacity`.
Con dec ≈ enc, el delta neto es lento. Si rate efectivo neto fuera
130 kps y cap 65536, llegar a 95 % = 62259 keys tomaría ~478 s,
extrapolado a saturación al minuto 8-9 (post-bootstrap completo).

### Hipótesis

1. **Bootstrap secuencial**: ORR pubkey-fetch + EstablishSecret toma
   ~30-50 s por par. Con 8 pares por orr, ~4-7 min en arrancar todos.
   El emit empezó en 00:02:21Z y vimos `master_secret pendiente`
   waits hasta ~00:03:00Z. Tiempo efectivo de emit: ~6 min, lo que
   da ~32k keys @ 150 kps. Consistente.

2. **Multipath está dividiendo el rate por commodity** (esperado),
   pero al mismo tiempo **bidireccional drain** lo enmascara para
   métrica de saturación. Las rates 30-170 kps son lo que el SDN
   asigna a CADA commodity individual; el flow agregado por par
   DKMS es el doble (suma 2 direcciones).

## Decisión iter_014

- **NO interrumpir sim 51**. Tiene timeout 600s, queda ~3 min.
- **NO lanzar OBJ-016 (random n=20 d=3) en paralelo**: violaría
  cgroup mental model de CLAUDE.md ("dos sims pesadas a la vez"
  satura EKS schedulers/etcd). Esperar a terminar mesh 3x3.
- **Cron próximo (00:13Z aprox)** recogerá la sim ya terminada o
  cerca: CSVs en `iter_013/post/pequena_densa/` + `sat_analysis.json`.
- Si timeout sin saturar, dkms-topo emite CSVs parciales con los
  buffers que SI llegaron a 0.95. Esto no es bloqueo: `bench_multipath`
  comparará lo que haya con baseline.

## Decisión sobre `MULTIPATH_ENABLED`

Confirmado vivo en orr-229 (verificado por env del pod via ORR_*
prefix). Las rates bimodales (30 vs 170 kps) son evidencia
indirecta. Si no fuera multipath, esperaríamos rates más planos
(264 kps fair-share). El reparto observado sugiere que SDN está
asignando ratios distintos a commodities según número de
caminos disponibles.

## Próximo

Iter_015 (siguiente cron):
1. Verificar PID 2597119 muerto y sim 51 status=FINISHED.
2. Inventariar archivos en `iter_013/post/pequena_densa/`:
   `sat_analysis.json`, payload, plots, loadtest_analysis.json.
3. Si CSVs presentes: cerrar OBJ-015, marcar [x] y arrancar OBJ-016
   (sim random n=20 d=3).
4. Si timeout sin CSVs: documentar fallo de saturación parcial
   (NO bloqueo R-016, no estamos ajustando params).

## Bloqueos

Ninguno. Solo espera natural.
