# Deployment

Every module is independently deployable. There's no required topology —
you can run only DKMS for a stand-alone ETSI surface, only QKC+quditto
for a per-link key generator, etc.

## Local dev

```bash
# Build everything
./scripts/build-all.sh

# Run a single module in the foreground
./scripts/run-sdn.sh
# ...in another terminal:
./scripts/run-qkc.sh
```

Each module reads its config from `<module>/config/default.toml` plus any
`local.toml` you drop next to it (gitignored). Env vars take final
precedence: `QKC_GRPC_ADDR=…`, `DKMS_HTTP_ADDR=…`, etc.

## Docker Compose (all-in-one)

```bash
cp .env.example .env
docker compose -f docker/docker-compose.yml up --build
```

## Kubernetes

Manifests per module under `<module>/k8s/`. There's no Helm chart yet —
the manifests are intentionally small, vanilla, and copy-pasteable.

```bash
# Deploy individual modules:
kubectl apply -f sdn/k8s
kubectl apply -f quditto/k8s
kubectl apply -f qkc/k8s
kubectl apply -f orr/k8s
kubectl apply -f dkms/k8s
```

### mTLS

Generate dev certs:
```bash
./scripts/gen-certs.sh
```

Mount the resulting `certs/` directory into each pod and point the
relevant config (`DKMS_TLS_CERT`, `DKMS_TLS_KEY`, `RUNTIME_MTLS_CA`).

## Observability

Each module exposes a Prometheus `/metrics` endpoint on its own port:

| Module  | Port |
|---------|------|
| qkc     | 9100 |
| orr     | 9101 |
| sdn     | 9102 |
| dkms    | 9103 |
| quditto | 9104 |

A shared `ServiceMonitor` resource is intentionally out of scope here.

## Web frontend

The Next.js web from the Python project is reused unchanged. See
[`web/README.md`](../web/README.md).
