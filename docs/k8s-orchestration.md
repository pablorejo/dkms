# k8s orchestration contract

Status: **draft / TBD**. This document is the negotiation surface between the
Python orchestrator (`dkms-sp-rr-saturated/code_dkms/src/k8s/`) and the Rust
runtime crates in this repo. The runtime is still in flux — fill the "Rust
(target)" columns as each module stabilizes.

## Decision

Orchestrator stays in Python (FastAPI + `kubernetes` client), runtime crates
are Rust. The orchestrator is not in the data plane, it's a deployer that
talks to the K8s apiserver; rewriting `pods.py` (~3.4k LOC) to `kube-rs`
buys nothing. The boundary is the env/port/endpoint contract below — when
that contract is stable, the Python orchestrator points `*_IMAGE` at the
Rust images and the rest of `pods.py` keeps working.

## Per-module port table

Authoritative once each module's `service.yaml` is updated to match.

| Module    | Python legacy ports               | Rust manifest (current)                    | Rust runtime config field            | Notes                                                          |
|-----------|-----------------------------------|--------------------------------------------|--------------------------------------|----------------------------------------------------------------|
| qkc       | grpc-ish on app port + range      | 50051 grpc, 7001 hot, 9100 metrics         | `peer_listen`, `local_listen`, `admin_http` | Rust QKC has no gRPC today — `peer_listen` is raw TCP, `admin_http` is the management HTTP. Manifest `grpc 50051` is a leftover. |
| orr       | (used QKC local socket)           | 50052 grpc, 9101 metrics                   | `grpc_addr`, `qkc_local_addr`, `metrics_addr` | ORR↔QKC is TCP to the co-located QKC `local_listen`, not gRPC. |
| sdn       | http only                         | 50053 grpc, 8081 http, 9102 metrics        | `grpc_addr`, `http_addr`, `metrics_addr`     | `/healthz` lives on http port.                                 |
| dkms      | 8080 + 4000–4100 range            | 8080 http, 50054 grpc, 9103 metrics        | `http_addr`, `grpc_addr`, `metrics_addr`     | `/readyz` + `/healthz` on http port. Range 4000–4100 likely gone. |
| quditto   | 5000 ETSI HTTP, 8000 metrics      | 50055 grpc, 9104 metrics                   | `listen` (HTTP ETSI 014)                     | Rust manifest says `grpc` but config exposes HTTP. Manifest needs fixing. |

Action items before plugging into the orchestrator:
- Drop or rename the leftover `grpc` ports in qkc/quditto manifests so they
  match the actual transport.
- Add HTTP probes once each module exposes `/healthz` and `/readyz` (today
  most use `tcpSocket`).
- Decide ack-socket / hot-path port offsets (`QKC_SOCKET_PORT_OFFSET`,
  `DKMS_ACK_SOCKET_PORT_OFFSET` in Python) — do they survive in Rust, or do
  we use a single port?

## Config delivery

Big divergence. Python `pods.py` injects ~60 env vars per pod. Rust modules
read a single `--config <file>` TOML (sometimes layered via
`common::config::load`) and ignore env vars beyond `RUST_LOG` + `CONFIG_DIR`.

Two paths, pick one:

- **(A) ConfigMap with rendered TOML**. Orchestrator builds the TOML
  per-simulation, mounts it under `$CONFIG_DIR/default.toml`. Pros: keeps
  Rust idiomatic, single source of truth. Cons: every Python env var
  becomes a TOML field, and per-link arrays (qkc `[[links]]`,
  orr `[peers]`) need orchestrator-side rendering.
- **(B) Env-var overrides on top of TOML**. Extend `common::config::load`
  to layer `MODULE_*` env vars (it already documents this in `CLAUDE.md`
  but isn't wired). Pros: less rendering work in `pods.py`. Cons: nested
  values (links, peers) don't map cleanly to env.

Recommendation: **A for structural config (links, peers, addresses),
B for tunables (timeouts, feature flags)**. That matches what each side
expresses naturally.

## Env vars the Python orchestrator injects today

Sourced from `code_dkms/src/k8s/pods.py`. Mark each as: **keep** (Rust will
read it), **drop** (no equivalent), **rename** (different name in Rust), or
**TBD**.

### QKC / per-link

| Python env                                    | Status | Rust equivalent           |
|-----------------------------------------------|--------|---------------------------|
| `QKC_ENABLE_TOKEN_BUCKET`                     | TBD    |                           |
| `QKC_SOCKET_PORT_OFFSET`                      | TBD    |                           |
| `QKC_SERVER_DECRYPT_RETRY_WINDOW_SECONDS`     | TBD    |                           |
| `QKC_RELAY_FORWARD_TIMEOUT_SECONDS`           | TBD    |                           |
| `QKC_SEND_TIMEOUT_SECONDS`                    | TBD    |                           |
| `QKC_DEC_KEYS_RETRY_WINDOW_SECONDS`           | TBD    |                           |

### DKMS

| Python env                                    | Status | Rust equivalent           |
|-----------------------------------------------|--------|---------------------------|
| `DKMS_CONFIG` (path to `DKMS/<id>.json`)      | drop   | `--config` TOML           |
| `BIND_IP` / `AGENT_CONTROLLER_PORT`           | rename | `http_addr` / `grpc_addr` |
| `CONFIG_FOLDER`                               | rename | `CONFIG_DIR`              |
| `KME_HTTP_TIMEOUT_SECONDS`                    | TBD    |                           |
| `KME_ENC_KEYS_REQUEST_TIMEOUT_SECONDS`        | TBD    |                           |
| `KME_ENC_KEYS_RETRY_WINDOW_SECONDS`           | TBD    |                           |
| `KME_DEC_KEYS_RETRY_WINDOW_SECONDS`           | TBD    |                           |
| `KME_DEC_KEYS_RETRY_INTERVAL_SECONDS`         | TBD    |                           |
| `PQC_SOCKET_TIMEOUT_SECONDS`                  | TBD    |                           |
| `DKMS_ACK_SOCKET_PORT_OFFSET`                 | TBD    |                           |

### SDN

| Python env                                    | Status | Rust equivalent           |
|-----------------------------------------------|--------|---------------------------|
| `SDN_CONFIG`                                  | drop   | `--config` TOML           |
| `SDN_BIND_IP` / `SDN_PORT`                    | rename | `http_addr`               |
| `SDN_ENABLE_DKMS_METRICS_LINK_STATE_SYNC`     | TBD    |                           |
| `SDN_ENABLE_QKC_LINK_CAPACITY_SYNC`           | TBD    |                           |
| `SDN_LSP_ZOMBIE_SWEEPER_ENABLED`              | TBD    |                           |
| `SDN_TOPOLOGY_JSON_B64`                       | TBD    | maps to `topology_file`?  |

### Quditto sidecar

| Python env                                    | Status | Rust equivalent           |
|-----------------------------------------------|--------|---------------------------|
| `QUDITTO_PORT` (default 5000)                 | rename | `listen`                  |
| `QUDITTO_METRICS_PORT`                        | TBD    |                           |
| `QUDITTO_WORKERS`                             | drop?  | Rust quditto is single-process |
| `QUDITTO_MAX_BUFFER_SIZE`                     | rename | `max_buffer_keys`         |
| `QUDITTO_RATE_R0`                             | rename | `r0`                      |
| `QUDITTO_RATE_ALPHA`                          | rename | `alpha`                   |
| `QUDITTO_DEFAULT_TTL`                         | TBD    |                           |
| `QUDITTO_VERBOSE`                             | rename | `RUST_LOG`                |
| `QUDITTO_INSECURE`                            | TBD    |                           |
| `QUDITTO_LOCAL_URL`                           | TBD    |                           |
| `QUDITTO_REQUEST_TIMEOUT_PER_KEY_SECONDS`     | TBD    |                           |
| `QUDITTO_DEC_KEYS_TIMEOUT_SECONDS`            | TBD    |                           |
| `QUDITTO_DEC_KEYS_TIMEOUT_MARGIN_SECONDS`     | TBD    |                           |

### Cross-module (image/ingress)

These the orchestrator consumes itself, not the runtime. Listed for
completeness.

- `SDN_IMAGE`, `DKMS_IMAGE`, `QUDITTO_IMAGE` — orchestrator points these at
  the Rust images. **No runtime change needed.**
- `K8S_IMAGE_PULL_SECRET`, `K8S_IMAGE_PULL_POLICY` — keep as-is.
- `K8S_INGRESS_*`, `K8S_RUNTIME_MTLS_*` — keep as-is.

## HTTP endpoints the orchestrator polls

`pods.py` / `orchestator.py` periodically calls into running runtime pods
for control-plane sync. Each one needs a Rust counterpart, or the
orchestrator needs to be told to disable that sync for the Rust deployment.

| Endpoint (Python)               | Caller         | Purpose                         | Rust status                                            |
|---------------------------------|----------------|---------------------------------|--------------------------------------------------------|
| `GET /metrics` on DKMS          | SDN runtime    | link-state sync from DKMS       | TBD — Rust DKMS exposes Prometheus on `metrics_addr`, but the Python SDN expects a custom JSON shape, not Prom text. |
| `GET /link_capacity` on QKC     | SDN runtime    | token-bucket capacity estimates | TBD — Rust QKC has no equivalent endpoint yet.        |
| `GET /healthz` / `GET /readyz`  | kubelet        | probes                          | DKMS+SDN have these; QKC/ORR/quditto use `tcpSocket`. |
| `POST /forwarding-table`?       | SDN→QKC        | route push                      | QKC `admin_http` exists — confirm schema matches.     |

The Python SDN consumes its own pods' `/metrics` and `/link_capacity` —
that's an internal coupling between Python SDN and Python DKMS/QKC. With
Rust SDN replacing Python SDN, this becomes a Rust↔Rust contract over gRPC
and the HTTP versions can be dropped. **But** if the orchestrator itself
also polls them for health/dashboards, we need to confirm.

## mTLS runtime CA

`code_dkms/src/k8s/runtime_ca.py` creates a CA per simulation, syncs the
cert bundle into a Secret in the simulation namespace, mounts it into pods
under `/app/certs/`. Pods read `client.crt` + `client.key` for mTLS to
peers and present a server cert signed by the same CA.

Rust runtime:
- `DkmsConfig.tls` already has `cert_path`, `key_path`, `client_ca` — good.
- QKC / ORR / quditto **have no TLS config struct yet** — TBD whether the
  orchestrator should keep injecting certs or whether the data plane is
  plain inside-cluster (NetworkPolicy + service mesh).

Decision needed once QKC/ORR mTLS posture is fixed.

## Observability sidecars

Python orchestrator deploys Prometheus, Loki, Grafana, and a promtail
sidecar per simulation (gated by `K8S_OBSERVABILITY_ENABLED`, default on).
No Rust changes needed — Rust runtime exposes Prometheus text on
`metrics_addr` and writes structured tracing to stdout, which is what
promtail picks up.

## Open questions

1. ConfigMap-rendered TOML vs env-var overrides? (See "Config delivery".)
2. Does the orchestrator need a runtime gRPC client to talk to Rust SDN/DKMS
   for status, or do we keep an HTTP shim on the Rust side?
3. What happens to the Python `simple-quditto` image — replaced by Rust
   quditto, or both live in parallel for A/B comparison?
4. NodeID vs namespace naming: Python `pods.py` derives service names like
   `dkms-13`. Rust crates assume operator-provided names in `node_id`.
   Orchestrator needs to generate `node_id` consistently with the service
   name so DNS resolution works.

## Inventory of static-manifest divergences

Things in `<m>/k8s/deployment.yaml` that will need fixing before the
orchestrator can deploy these images:

- **qkc**: manifest port `grpc 50051` doesn't match config (no gRPC).
  Should be `peer 7001`, `local 7100`, `admin 7200` or whatever the config
  ends up using.
- **quditto**: manifest port `grpc 50055` doesn't match config (HTTP ETSI).
  Should be `http <listen port>`.
- **probes**: most use `tcpSocket`; once HTTP `/healthz` exists, switch.
- **image refs**: all point to `ghcr.io/your-org/...`. Orchestrator uses
  `docker.io/pablopio/...`. The orchestrator sets the image, so the
  manifest defaults only matter for `kubectl apply` outside the
  orchestrator.
- **resource limits**: arbitrary today. Once loadtest baselines exist,
  tune.

These are static-manifest fixes, separate from the orchestrator contract,
but worth tracking together so we don't ship divergent k8s definitions.
