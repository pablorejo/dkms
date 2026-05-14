# Kubernetes deployment for `web/`

## Purpose

This directory contains the Kubernetes manifests and helper script used to
deploy the web frontend in namespace `web-dkms`.

## What it contains

- `namespace.yaml`
- `deployment.yaml`
- `service.yaml`
- `ingress.yaml`
- `deploy.sh`

## Defaults

- namespace: `web-dkms`
- replicas: `2`
- ingress class: `nginx`
- ingress path: `/web`

## Key inputs

Required or commonly used variables:

- `DOCKER_HUB_USERNAME`
- `DOCKER_HUB_TOKEN`
- `WEB_NAMESPACE`
- `WEB_IMAGE`
- `WEB_INGRESS_HOST`
- `WEB_INGRESS_CLASS`
- `WEB_INGRESS_PATH`
- `WEB_REPLICAS`
- `WEB_ORCH_COOKIE_SECURE`
- `WEB_ORCH_REQUEST_TIMEOUT_MS`
- `WEB_RUNTIME_BASE_URL`

## Deploy

```bash
bash web/k8s/deploy.sh
```

The deploy script also performs a rollout restart so Kubernetes pulls the new
image even when a mutable tag such as `v1` or `latest` is reused.

## Related docs

- [../README.md](../README.md)
- [../../docs/OPERATIONS.md](../../docs/OPERATIONS.md)
- [../../code_dkms/src/k8s/README.md](../../code_dkms/src/k8s/README.md)

## Known constraints

- This deployment assumes ingress-nginx and the current `/web` base path.
- When global HTTPS is enabled, `WEB_ORCH_COOKIE_SECURE` should remain `true`.
