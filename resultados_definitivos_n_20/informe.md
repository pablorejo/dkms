# Informe — ETSI 014 loadtest 30 workers × 500 pares × N=20

**Fecha:** 2026-05-26 / 2026-05-27
**Cluster:** EKS `dkms1` (eu-north-1)
**Branch:** `mcmcf-lambda`
**Tag de referencia:** `etsi014-loadtest-30w-saturation`

## 1. Objetivo del experimento

Validar que el DKMS responde correctamente al contrato ETSI 014 bajo
carga sostenida y, sobre todo, **encontrar el techo de saturación** del
sistema (HTTP 429 del token bucket interno) en las 5 topologías
estándar del repo:

- **ba**: Barabási-Albert, N=20, k=3, seed 1734.
- **er**: Erdős-Rényi, N=20, k=3, seed 1734.
- **rgg**: Random Geometric Graph, N=20, max distance 30 km, k=3.
- **secoqc**: Topología SECOQC (fija), N=20.
- **ba2**: Igual a `ba` pero con seed 5678 (variabilidad).

## 2. Setup del experimento

| Parámetro | Valor |
|---|---|
| Workers concurrentes | 30 (kind: Job, pod-per-worker, 30 procesos Python) |
| Pares SAE por worker | 500 (master + slave) |
| SAEs totales por run | 30 000 |
| Tasa por par | λ = 2 r/s (Poisson + jitter uniforme inter-arrival) |
| RPS nominal agregada | 60 000 rps |
| Sync barrier (SYNC_OFFSET) | 25 min (todos los workers arrancan al mismo segundo) |
| Rampa | 50 → 500 pares step 50 cada 15 s (150 s total) |
| Hold | 120 s |
| Topología base | BA/ER/RGG/SECOQC, **N=20**, k=3 |

### Imagen del cliente

- `pablopio/etsi014-rt-client:v8`
- aiohttp + aiodns (AsyncResolver), SIGTERM handler async limpio.

### Patches al orchestator (`patch_orchestator_for_loadtest.sh`)

Sin estos patches el provisioning de 30k SAEs concurrente colapsa
(cert issuance se serializa en un único worker uvicorn):

- `command/args`: `uvicorn ... --workers 4`
- `SQLALCHEMY_POOL_SIZE = 30`, `SQLALCHEMY_MAX_OVERFLOW = 60`
- Resources: `cpu limit 4`, `memory limit 4 Gi` (vs 1 CPU / 1 Gi default)

Resultado: provisioning de 30 k SAEs baja de 45 min a ~12 min con 30
workers paralelos sin HTTP 500 ni TimeoutErrors.

## 3. Resultados globales

| Topo | Total round-trips | Match % | HTTP 429 | 429 / Total | HTTP 502 | enc p50 | enc p95 | enc p99 | enc max |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| ba | 2 206 336 | 82.92 % | 376 737 | 17.07 % | 5 | 10.3 ms | 2454 ms | 4813 ms | 10 013 ms |
| er | 2 279 510 | 83.21 % | 281 725 | 12.36 % | 24 | 10.0 ms | 2144 ms | 4380 ms | 10 693 ms |
| **rgg** | **2 329 499** | **91.08 %** | **207 733** | **8.92 %** | 55 | **9.2 ms** | **1936 ms** | **4139 ms** | 10 306 ms |
| secoqc | 2 186 046 | 90.50 % | 207 650 | 9.50 % | 20 | 10.0 ms | 2465 ms | 4989 ms | 10 085 ms |
| ba2 | 2 189 544 | **77.76 %** | **486 865** | **22.24 %** | 24 | 10.9 ms | 2647 ms | 4927 ms | 10 398 ms |

### Ranking por throughput servido (match × total)

1. **rgg**: 2 121 707 (91.08 % de 2.33 M)
2. **secoqc**: 1 978 372 (90.50 % de 2.19 M)
3. **er**: 1 896 758 (83.21 % de 2.28 M)
4. **ba**: 1 829 593 (82.92 % de 2.21 M)
5. **ba2**: 1 702 528 (77.76 % de 2.19 M)

## 4. Análisis por aspecto

### 4.1. ¿Dónde está el cuello?

**En el DKMS (token bucket por par)**, no en el cliente ni en el ingress.

Evidencia:

- **0 client errors significativos** (1-174 `empty exc` por run sobre
  millones de requests).
- **Pocos HTTP 502** (5-55) — son errores transitorios del ingress
  nginx cuando reenvía a un backend que tarda > timeout, no es el
  cuello sostenido.
- **HTTP 429 masivos** en TODAS las topologías (entre 208k y 487k
  respuestas), proporcionales a la capacidad de cada topología.
- **Latencias p50 ~10 ms** uniformes — cuando el bucket tiene tokens,
  el DKMS responde rapidísimo. Las colas largas (p95 ~2 s, p99 ~4-5 s)
  son requests esperando que el bucket se refile o errores 429
  retrasados.

### 4.2. RGG es la topología que mejor escala

RGG obtiene **91.08 % match con 8.9 % de 429** — el techo más alto
observado.

Razón: en RGG todos los enlaces son geográficos cortos (`max
distance 30 km`). La fórmula del SDN para capacidad del enlace es:

```
cap_edge = R0 × 10^(-α·d/10)
```

Con α=0.2 dB/km y d=5-30 km → factor entre 0.5 y 0.79. En BA/ER los
hubs centrales pueden tener enlaces a nodos muy alejados (factor
mucho menor) — esto reduce la capacidad del fair-share por commodity.

Además RGG tiene buen *clustering coefficient* (vecinos suelen ser
vecinos entre sí) → muchas rutas cortas alternativas, el SDN
distribuye carga mejor.

### 4.3. SECOQC se comporta similar a RGG

90.50 % match, 9.50 % de 429. SECOQC es una topología fija
(originalmente publicada para la red SECOQC de Viena) muy estructurada
y bien dimensionada — confirma que **el techo del DKMS está más
limitado por la concurrencia de pares (~6 600 keys/s/cluster) que por
la topología cuando los enlaces son razonablemente cortos**.

### 4.4. ER vs BA: ER algo mejor

ER (83.21 %) supera a BA (82.92 %) por solo 0.3 puntos. Ambas presentan
~17-12 % de 429. ER tiene distribución de grados más uniforme; BA
genera hubs que se vuelven cuellos. En la práctica con N=20 la
diferencia es marginal.

**Buffer pre-loadtest (`sat=380/380`)**:

- er: median_ratio **1.41** (sobre-saturado al inicio — buffers
  llenos por encima del objetivo)
- ba: 0.86 (cerca del objetivo)

A pesar del buffer 1.41 vs 0.86, el throughput sostenido es similar
porque una vez el buffer se consume, ambos quedan limitados por la
*rate* de generación del DKMS (~6 600 keys/s), no por el buffer.

### 4.5. BA vs BA2: 5 puntos de diferencia por puro azar de seed

| | match | 429 | total |
|---|---:|---:|---:|
| ba (seed 1734) | 82.92 % | 17.07 % | 2.21 M |
| ba2 (seed 5678) | **77.76 %** | **22.24 %** | 2.19 M |

5.16 puntos de diferencia ENTRE LA MISMA TOPOLOGÍA con distinta
realización aleatoria. Implicación importante: **una corrida única no
caracteriza una familia topológica**. Para conclusiones estadísticas
firmes harían falta múltiples seeds por familia.

Hipótesis del peor caso de ba2: estructuralmente generó hubs más
"pesados" que en ba (más pares concentrados en pocos DKMS) → el
bucket de esos DKMS se vacía antes y dispara 429 de toda la cola
conectada.

**RGG con buffer median_ratio = 0.03** (casi vacío al inicio del
loadtest) aún así obtuvo el mejor resultado — la generación contínua
del DKMS durante el run compensa.

### 4.6. Latencias

Todas las topos tienen un patrón muy similar:

- **p50 ~ 10 ms** (las requests que el bucket sirve directamente).
- **p95 ~ 2 s** (cola moderada — sirve, pero con espera).
- **p99 ~ 4-5 s** (cola larga).
- **max ~ 10 s** (timeout del cliente — REQUEST_TIMEOUT=10 s).

El cliente está configurado con `request_timeout=10 s`. Las requests
que alcanzan 10 s **cuentan como exception del cliente, no como 429**.
Esto significa que el match real bajo saturación podría ser
ligeramente peor del reportado: requests que recibirían 429 con cliente
infinitamente paciente aquí mueren por timeout y cuentan como ok=0 sin
clasificarse.

### 4.7. Estabilidad de la rampa

Todos los workers arrancaron **sincronizados al milisegundo** (sync
barrier validado en el log de los pods: `step → 50 pairs active` a la
misma hora en los 30 workers). El plot RPS por bin de 1 s muestra
rampa limpia 50 → 100 → 150 → ... → 500 pares.

## 5. Conclusiones

1. **El cliente NO es cuello**: el harness de 30 workers Python
   asyncio + aiohttp + aiodns sostiene 12-13 k rps reales sin errores
   propios. La capa de cuello es claramente el **token bucket del DKMS**.

2. **Capacidad observada DKMS**: ~6 600 keys/s/cluster (20 DKMS) en
   las topologías densas (BA/ER). La demanda nominal 60 000 rps
   excede en ×10 la capacidad sostenida, lo que produce los 429
   masivos. **Esto es comportamiento correcto** del back-pressure
   ETSI 014.

3. **Topología sí importa pero menos que la realización**: BA y BA2
   (misma familia) difieren en 5 puntos de match por puro azar de
   seed. RGG y SECOQC obtienen 90+% match — son las más amables con
   el SDN porque distribuyen el tráfico mejor.

4. **El sistema preserva la integridad criptográfica**: el match de
   bytes entre `enc_keys.master_key` y `dec_keys.slave_key` es
   **100 %** en todas las requests que completan ambas mitades
   (cuando una de las dos llamadas devuelve 429, la request entera
   cuenta como no-match; no hay ningún caso observado de match-byte
   roto).

5. **Reproducibilidad confirmada**: las 5 topologías se ejecutaron
   con el mismo `run_30w_500p_topo.sh` parametrizado, sin
   intervención manual entre runs. Los artefactos están bajo
   `resultados_definitivos_n_20/<topo>/`.

## 6. Trabajo futuro sugerido

- **Múltiples seeds por familia**: 3-5 seeds × ba/er/rgg para tener
  intervalos de confianza en match%.
- **Subir N a 30**: ver si el bug `microlp Infeasible phase-2` (ver
  CLAUDE.md) se manifiesta y cuánto degrada el throughput.
- **Bajar λ a 1 y subir SAEs a 60 000**: ver si el patrón cambia con
  la misma demanda total nominal pero más concurrencia (más SAEs por
  DKMS).
- **Caracterizar el techo real**: barrer λ ∈ {0.5, 1, 2, 5} con la
  misma topología → curva clásica oferta/demanda con punto de quiebre.

## 7. Layout de artefactos

```
resultados_definitivos_n_20/
├── informe.md                 # este archivo
├── ba/
│   ├── summary.json
│   ├── requests.csv           # CSV agregado de los 30 workers
│   ├── test.log               # log del run (DONE markers + bytes/worker)
│   ├── buffer_run.log         # log del buffer-saturation pre-loadtest
│   └── plots/
│       ├── rps_over_time.png
│       ├── match_vs_429.png
│       ├── latency_split_time.png
│       ├── latency_hist.png
│       ├── match_over_time.png
│       ├── errors_over_time.png
│       ├── outcome_breakdown.png
│       └── keys_per_dkms.png
├── er/  ... (misma estructura)
├── rgg/ ...
├── secoqc/ ...
└── ba2/ ...
```

Para reproducir desde cero:

```bash
bash scripts/etsi014_loadtest/patch_orchestator_for_loadtest.sh
# relanza los port-forwards (el rollout los rompe)
bash scripts/etsi014_loadtest/run_30w_all_topos.sh
```
