# ETSI 014 round-trip loadtest

Loadtest distribuido del DKMS via ingress nginx + mTLS, verificando el
contrato ETSI 014 completo (master `enc_keys` + slave `dec_keys` con match
de bytes).

Probado en EKS cluster `dkms1` (eu-north-1), topo BA N=20, hasta 30 000
SAEs (15 000 pares) y 60 000 rps nominales — encuentra el techo de
saturación del DKMS (HTTP 429 token bucket) limpiamente.

## Layout

| Archivo | Función |
|---|---|
| `etsi014_roundtrip_loadtest.py` | Cliente Python asyncio — provisiona SAEs, hace ramp + hold de round-trips a tasa Poisson con jitter uniforme, escribe CSV incremental. Empaquetado en `pablopio/etsi014-rt-client:v8`. |
| `docker/Dockerfile` | Imagen del cliente (python:3.12-slim + aiohttp + aiodns). |
| `bd_cleanup.py` | Limpia BD del orchestator (SAEs huérfanos + sims `finished`) entre runs. |
| `patch_resources.sh` | Hook `--post-run-hook` de `dkms_topo` para tunear DKMS sidecars antes de la saturación. |
| `patch_orchestator_for_loadtest.sh` | **Aplica los patches al orchestator** (uvicorn workers=4, QueuePool 30/60, CPU 4 / RAM 4Gi). Sin esto, provisioning >15 workers concurrentes colapsa el orchestator. |
| `run_jitter_v8_synced.sh` | Run "calibración" — 15 workers × 500 pares. ~7 500 SAEs. Sirve para validar cluster sin saturación del DKMS. |
| `run_30w_500p.sh` | Run **saturación** — 30 workers × 500 pares. 15 000 pares = 30 000 SAEs, λ=2 → 60 000 rps nominal. Encuentra HTTP 429 del DKMS. |
| `aggregate_workers.py` | Junta los CSVs de los N workers + escribe `summary.json`. |
| `plot_roundtrip.py` `plot_error_breakdown.py` `plot_match_vs_429.py` | Generan los PNGs estándar (`rps_over_time.png`, `match_vs_429.png`, etc). |

## Prerrequisitos

1. `kubectl config current-context` debe acabar en `cluster/dkms1`.
2. Port-forwards activos:
   ```bash
   kubectl -n dkms-main-ns port-forward deploy/orchestator 18080:8080 &
   kubectl -n dkms-main-ns port-forward deploy/authz 18081:8081 &
   ```
3. Imagen del cliente publicada: `pablopio/etsi014-rt-client:v8`. Si tocas
   el script Python, rebuilda:
   ```bash
   cd scripts/etsi014_loadtest/docker
   cp ../etsi014_roundtrip_loadtest.py .
   docker build -t pablopio/etsi014-rt-client:v9 .
   docker push pablopio/etsi014-rt-client:v9
   # y bumpear el tag en run_*.sh
   ```
4. **Aplicar patches al orchestator** (idempotente):
   ```bash
   bash scripts/etsi014_loadtest/patch_orchestator_for_loadtest.sh
   # ↑ relanza el rollout y rompe los port-forwards; relánzalos después
   ```

## Lanzar un run

Desde la raíz del repo:

```bash
bash scripts/etsi014_loadtest/run_30w_500p.sh
```

El script:
1. Cleanup BD + Jobs previos.
2. `dkms_topo ba -n 20 ...` crea un sim BA N=20 + corre buffer-saturation
   test.
3. Crea N **kind: Job** workers (no Deployment — Deployment re-spawn al
   exit del container y pierde el `emptyDir`).
4. Cada Job hace bulk provisioning de sus SAEs vía
   `/orch/admin/saes/bulk?issue_certs=true`, escribe DONE marker cuando
   completa ramp + hold, queda en `await shutdown_event` hasta `SIGTERM`.
5. Bash hace polling de DONE marker via `kubectl exec sh -c '[ -f DONE ]'`.
6. `kubectl cp` recupera el `requests.csv` mientras el Pod sigue Running.
7. `kubectl delete jobs` manda SIGTERM → cliente exit 0 → Pod Succeeded.
8. Aggregate + plots.

Artefactos en `tests/results/etsi014-30w-500p/`:
```
buffer/                      # logs DKMS del buffer-saturation test
sae/
├── worker-0/requests.csv    # CSV por worker
├── worker-1/requests.csv
├── ...
├── requests.csv             # CSV agregado
├── summary.json             # totales + percentiles + 429 count
└── plots/*.png
```

## Por qué Job (no Deployment)

El cliente Python tiene **dos modos de salida**:
- Exit 0 al final (provisioning + ramp + hold completos) → Pod Succeeded.
- SIGTERM externo durante el hold → flush limpio + exit 0 → Pod Succeeded.

Con `kind: Deployment` + `restartPolicy: Always`, el exit del container
re-spawn un Pod NUEVO con `emptyDir` vacío → CSV perdido. Con `kind: Job`
+ `restartPolicy: OnFailure`, exit 0 deja el Pod en Succeeded y el
`emptyDir` persiste hasta que se borre el Job. Eso permite `kubectl cp`
sin race.

## Por qué los patches al orchestator

Sin patches:
- 1 uvicorn worker + 1 CPU → cert issuance (EC P-256 keypair + sign,
  ~20 ms/SAE) serializa todo en un event loop con GIL.
- QueuePool 5+10 → 30 clientes paralelos pelean por 15 conexiones BD.
- Resultado: con 30 workers, batches de 200 SAEs tardan 400 s, hay
  HTTP 500 (pool exhausted) y TimeoutErrors. Provisioning de 30k SAEs no
  llega a 25 min de SYNC_OFFSET.

Con patches (4 uvicorn workers + 4 CPU + 30/60 QueuePool):
- 4 procesos Python paralelos para cert issuance, cada uno con su CPU.
- 90 conexiones BD disponibles → sin contención.
- Provisioning baja a ~12 min para 30k SAEs.

## Hallazgos

Punto reproducible (`run_30w_500p.sh`):

```
total_requests:  2 206 336
match_pct:       82.92 %
HTTP 429:        376 737  (17 % del total)
enc p50:         10 ms
enc p95:         2 454 ms
enc p99:         4 813 ms
```

- DKMS supply: ~6 600 keys/s (20 DKMS × 333 keys/s observados en buffer).
- Demanda nominal en plateau: ~30 000 rps (30 workers × ~1 000 rps each).
- DKMS sirve **~13 500 rps reales** durante 135 s (buffer pre-cargado se
  consume); luego empieza HTTP 429 sostenido.
- Match cae a 82.9 % porque el 17 % de 429 cuenta como round-trip fallido.
