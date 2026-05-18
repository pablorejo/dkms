# `dkms-topo` — CLI local para lanzar simulaciones DKMS contra EKS

CLI Python que orquesta simulaciones DKMS contra el cluster EKS `dkms1`
(eu-north-1, namespace `dkms-main-ns`). Genera 5 familias de topologías
programáticamente, las envía a la HTTP API del `orchestator`, captura
logs vía `kubectl logs -f`, compara las tasas observadas contra un
modelo teórico max-min idéntico semánticamente al solver
`sdn/src/mcf.rs::weighted_maxmin` y, opcionalmente, encadena una rampa
de SAEs con análisis de métricas Prometheus.

**Estado actual**: implementación completa offline (Fases A–E + G).
La Fase F (smoke real en EKS) está bloqueada por un bug pre-existente
del orchestator — ver §Troubleshooting → "create_simulation → HTTP 500".

## Pre-requisitos

### Software local

| Dep         | Versión mínima | Estado típico |
|-------------|----------------|---------------|
| Python      | 3.10           | Arch / Debian moderno |
| `kubectl`   | 1.27           | con plugin AWS |
| `requests`  | 2.31           | sistema o venv |
| `matplotlib`| 3.8            | sistema o venv |
| `pytest`    | 8.0            | sistema o venv |

`pydantic` y `typer` están listados en `requirements.txt` como
**opcionales** — la CLI usa `argparse` stdlib (ver
`.architecture.md` historial 2026-05-18 iter 017).

### Acceso al cluster EKS

1. **Contexto kubectl** debe terminar en `cluster/dkms1`:
   ```bash
   kubectl config current-context
   # arn:aws:eks:eu-north-1:415984013031:cluster/dkms1
   ```

2. **Port-forwards** activos contra `dkms-main-ns`:
   ```bash
   kubectl -n dkms-main-ns port-forward svc/authz       18081:8081 &
   kubectl -n dkms-main-ns port-forward svc/orchestator 18080:8080 &
   ```

3. **Usuario authz**: la CLI usa `_login_or_register`, así que crea el
   usuario en su primera llamada. Si el username pre-existe con otra
   contraseña, fallará — usa `--username AGENT_NAME --password ...`
   con un nombre dedicado:
   ```bash
   python3 -m tests.cli.dkms_topo ring -n 4 --buffer-saturated \
       --username dkms-topo-agent --password agent-pw-2026
   ```

## Instalación

### Opción A — Python del sistema (recomendado)

`argparse` es stdlib; `requests` y `matplotlib` suelen estar
instalados. Comprueba con:
```bash
python3 -c "import requests, matplotlib; print('ok')"
```

### Opción B — venv local

```bash
cd tests/cli
python3 -m venv .venv
. .venv/bin/activate
pip install -r requirements.txt
```

`.venv/` está en `.gitignore` (entrada genérica de la raíz).

## Uso

Todas las invocaciones son `python3 -m tests.cli.dkms_topo SUBCMD ...`
desde la raíz del repo.

### Subcomandos (topología)

| Subcmd  | Flags propios               | Nodos generados |
|---------|-----------------------------|-----------------|
| `ring`  | `-n N` (n ≥ 3)              | N               |
| `line`  | `-n N` (n ≥ 2)              | N               |
| `mesh`  | `-n N -m M` (n,m ≥ 2)       | N·M             |
| `star`  | `-p PER -b BRANCHES`        | 1 + B·P         |
| `random`| `-n N -d AVG_DEGREE [--seed S]` | N           |

### Flags globales (todos los subcomandos)

| Flag                         | Default | Descripción |
|------------------------------|---------|-------------|
| `--name STR`                 | auto    | Nombre de la sim (auto-deriva del subcmd si se omite) |
| `--owner UID`                | None    | uid del propietario (se inyecta en `description`; el header `X-User-Id` carga la autenticación real) |
| `--r0 FLOAT`                 | 2000.0  | R0 por link (keys/s) |
| `--alpha FLOAT`              | 0.0     | α emitido en el payload — el orchestator IGNORA `α=0` y aplica 0.2 internamente (bug R-011) |
| `--distance INT`             | 5       | distance_km por link |
| `--buffer-enc-size INT`      | 65536   | Capacidad del buffer ENC por peer |
| `--sdn-endpoint STR`         | 172.30.0.2:3000 | `ip:port` o `http(s)://host:port` |
| `--effective-alpha FLOAT`    | 0.2     | α usado para el cálculo TEÓRICO (cubre R-011) |
| `--dry-run`                  | off     | Imprime el JSON del payload y sale; sin EKS |
| `--buffer-saturated`         | off     | Crea sim → run → tail logs → analiza saturación → para sim |
| `--sae-test`                 | off     | Tras saturación, lanza rampa SAE y analiza métricas |
| `--no-fill`                  | off     | Salta la fase de saturación; va directo a `--sae-test` |
| `--output-dir PATH`          | `tests/results/<utc>-<topo>/` | Directorio para outputs |
| `--saturation-timeout SEC`   | 600     | Timeout absoluto para alcanzar saturación |
| `--sat-threshold FRAC`       | 0.95    | enc ≥ frac × buffer → saturado |
| `--authz-url URL`            | http://127.0.0.1:18081 | URL del port-forward de authz |
| `--orch-url URL`             | http://127.0.0.1:18080 | URL del port-forward del orchestator |
| `--username STR`             | config_user | Username authz |
| `--password STR`             | config_user | Password authz |
| `--namespace STR`            | sim-<id>-ns | k8s namespace |
| `--force` / `--yes`          | off     | Borra sims previas con el mismo nombre sin preguntar |

### Flags adicionales para `--sae-test`

| Flag                         | Default | Mapeo en `LoadTestCreateRequest` |
|------------------------------|---------|----------------------------------|
| `--sae-start INT`            | 5       | `start_saes` |
| `--sae-end INT`              | 50      | `end_saes` |
| `--sae-step INT`             | 5       | `step_saes` |
| `--time-step FLOAT`          | 15.0    | `interval_seconds` |
| `--sae-warmup FLOAT`         | 30.0    | `warmup_seconds` |
| `--lambda-sae FLOAT`         | 0.5     | `per_sae_lambda` |
| `--sae-key-size INT`         | 256     | `key_size_bits` |
| `--sae-request-timeout INT`  | 60      | `request_timeout_seconds` |
| `--loadtest-duration FLOAT`  | auto    | Wall-clock; si omitido, `warmup + ((end-start)/step) × interval + 60s` |

## Ejemplos

### Dry-run (sin contactar EKS)

```bash
# Anillo de 4 nodos, payload por stdout
python3 -m tests.cli.dkms_topo ring -n 4 --dry-run

# Malla 3×3 con R0 personalizado, payload a fichero
python3 -m tests.cli.dkms_topo mesh -n 3 -m 3 --r0 1500 --dry-run \
    > /tmp/mesh-3x3.json
```

### Saturación en EKS (flujo principal)

```bash
python3 -m tests.cli.dkms_topo ring -n 4 \
    --buffer-saturated \
    --force \
    --username dkms-topo-agent --password agent-pw-2026 \
    --saturation-timeout 600 \
    --output-dir tests/results/my-ring-4/
```

Esto genera el payload, lo POST-ea al orchestator, lanza `kubectl
logs -f` por cada pod DKMS, espera hasta que TODOS los commodities
alcancen `enc ≥ 0.95 × buffer`, ejecuta `analyze_saturation` y
escribe `tests/results/my-ring-4/sat_analysis.json`. Para la sim
al final independientemente del resultado (R-008).

### Saturación + rampa SAE encadenada

```bash
python3 -m tests.cli.dkms_topo star -p 2 -b 3 \
    --buffer-saturated --sae-test \
    --sae-start 10 --sae-end 100 --sae-step 10 \
    --time-step 15 --lambda-sae 1.0 \
    --force --username dkms-topo-agent --password agent-pw-2026
```

### Sólo rampa SAE (skip saturación)

```bash
python3 -m tests.cli.dkms_topo ring -n 4 \
    --sae-test --no-fill \
    --sae-start 100 --sae-end 500 --sae-step 50 \
    --lambda-sae 5.0 \
    --force --username dkms-topo-agent --password agent-pw-2026
```

Útil para forzar 429s sin esperar a la fase de fill.

## Outputs

Cada corrida produce un directorio (defecto
`tests/results/<utc-iso>-<name>/`) con:

```
tests/results/2026-05-18T20-15-30-ring-n4/
├── payload.json                # body enviado a /orch/web/simulations
├── dkms-3-9d87....log          # uno por pod DKMS (kubectl logs -f)
├── ...
├── sat_analysis.json           # output de analyze_saturation
├── loadtest.log                # si --sae-test (kubectl logs deploy/loadtest)
├── loadtest-metrics.txt        # si --sae-test (Prometheus text)
├── loadtest_analysis.json      # si --sae-test (counts + percentiles)
└── plots/
    ├── requests_by_status.png  # bar chart (sólo --sae-test)
    └── latency_percentiles.png # CDF log-X con líneas p50/p90/p95/p99
```

`tests/results/` está en `.gitignore`. Los plots usan `matplotlib`
con backend `Agg` (headless OK).

## Códigos de salida

| Code | Significado |
|-----:|-------------|
| 0    | Todo OK; saturación completa (si aplicaba) y loadtest exitoso (si aplicaba) |
| 1    | Error inesperado (excepción en builder, en EKS path, etc.) |
| 2    | Argumentos insuficientes (sin `--dry-run`/`--buffer-saturated`/`--sae-test`) |
| 3    | Timeout antes de saturación completa (la sim igual se para) |
| 4    | Scrape de métricas Prometheus falló (los logs sí se capturan) |
| 5    | Usuario declinó borrar sim previa con el mismo nombre (sin `--force`) |

## Tests

```bash
# Suite unitaria completa (no EKS, no port-forwards, ~0.4s)
python3 -m pytest tests/cli/

# Sólo integration (cuando existan; requieren EKS)
python3 -m pytest tests/cli/ -m integration

# Marker decorativo desde iter 022 — los integration tests reales
# se añadirán cuando se desbloquee la Fase F.
```

## Modelo teórico

Ver `tests/theorical_results.md` para:
- Fórmulas cerradas ring/line/star.
- Snippet del algoritmo max-min (idéntico al Rust).
- Limitaciones (single-path BFS, peso 1.0 uniforme, etc.).

## Bug documentado: `α=0` ignorado por el orchestator

El orchestator IGNORA el campo `quditto_rate_alpha=0` del payload y
aplica `α=0.2` por defecto (bug pre-existente fuera del alcance de
esta CLI — ver `CLAUDE.md` "Tasa teórica fair-share — NO es R0"
para detalles). En consecuencia:

- La CLI emite por defecto `α=0.0` en el payload (transparente).
- El cálculo teórico usa `--effective-alpha 0.2` (lo que realmente
  aplica el cluster).
- Si en el futuro el bug se arregla, pasa `--effective-alpha 0.0`.

## Troubleshooting

### `AuthenticationError: Invalid credentials` al login

`config_user` puede existir en authz con otra contraseña que la
default `config_user`. Workaround: usa un username dedicado del
agente:

```bash
--username dkms-topo-agent --password agent-pw-2026
```

`_login_or_register` registra automáticamente si el user no existe.

### `ServerError: HTTP 500` al `create_simulation`

Síntoma:
```
sqlalchemy.exc.IntegrityError: (psycopg2.errors.NotNullViolation)
  null value in column "local_qkc_id" of relation "kme"
```

Causa: **colisión de `local_qkc_id` con KMEs de una sim anterior**
en la misma BD del orchestator. El orchestator computa
`local_qkc_id = 100000 + node_id`; dos sims con node_ids solapados
chocan en el UPSERT.

**Workaround** (implementado): pasar `--node-id-offset N` para
shift-ear los node_ids fuera del rango ocupado:

```bash
python3 -m tests.cli.dkms_topo ring -n 4 --buffer-saturated \
    --node-id-offset 30 ...
```

Rango seguro: `[17..51]`. Límite superior por la check constraint
IPv4 del orchestator: `kme_host = 172.30.0.{200 + node_id}` debe
ser ≤ 255, así que `node_id ≤ 55`. Si la sim de fondo usa node_ids
1..16, los offsets 17..51 garantizan no-colisión y validez IPv4.

### CLI saturó pero `--sae-test` falla con `container not found`

Síntoma:
```
[dkms-topo] sat=12/12 median_ratio=1.018
[dkms-topo] loadtest scrape FAILED: kubectl exec metrics scrape
  failed: error: Internal error occurred: unable to upgrade
  connection: container not found ("loadtest")
```

Causa raíz (al inspeccionar `tests/results/<run>/loadtest.log`):

```
RuntimeError: Error consultando
  https://dkms2.pablopiorejoiglesias.es/api/sim/<id>/sdn/dkms/
  (HTTP 403): nginx 403 Forbidden
```

El pod `dkms-loadtest:v1` del orchestator resuelve los DKMS endpoints
via el ingress público con un service-token; recibe 403 → crashea
antes de bind del endpoint :9095/metrics → `kubectl exec` no puede
unirse al container.

**Estado**: bloqueo a nivel de cluster/orchestator (ingress mTLS /
authz), fuera del alcance de la CLI (R-002 prohíbe modificar
`orchestrator/*` y `k8s/`).

Solución posible (no aplicada aquí): reconfigurar el ingress
`runtime-ingress` para admitir el service-token del pod loadtest, o
crear un user authz con rol `service` que la CLI use por defecto.

### CLI cuelga en `kubectl logs -f`

Si una sim arranca pero un pod nunca llega a `Available`, el
`tail_pods` se queda esperando logs. Mata con SIGINT; el cleanup
del finally para la sim igual.

### Logs corruptos / ANSI codes en `sat_analysis.json`

Falsa alarma — `sat_parser.strip_ansi` limpia los códigos antes de
parsear. Verifica con:

```bash
grep "generator.state" tests/results/<run>/dkms-*.log | head
```

Las líneas deben tener `peer=node-X enc=Y ...` legibles.

### Marker `integration` no reconocido

`tests/cli/pyproject.toml` registra el marker. Asegúrate de correr
pytest **desde la raíz del repo** (no desde `tests/cli/`) para que
el config sea encontrado.

## Restricciones de diseño

Ver `agent-dkms-topo-cli/.restrict.md` para el listado completo. Las
más relevantes:

- **R-005**: para K8s, sólo `subprocess` → `kubectl get|logs|wait|exec`.
  PROHIBIDO `kube-rs`, Python K8s client, `client-go`.
- **R-008**: cada sim creada se para con `stop_simulation` antes de
  cerrar (`finally`).
- **R-011**: el bug `α=0` se documenta pero NO se arregla aquí (vive
  en `orchestrator/`).
- **R-012**: confirmación interactiva por defecto al borrar sims;
  `--force` (alias `--yes`) lo salta.
- **Saturation/load tests** SOLO en EKS, NUNCA en local (R-004).
