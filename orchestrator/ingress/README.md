# Kubernetes Ingress Manifests

## Purpose

This directory contains the ingress and deployment manifests used to expose
AuthZ, Orchestrator, DKMS, and SDN traffic through ingress-nginx.

## What it contains

- `authz-deployment.yaml`
- `ingress-authz.yaml`
- `orchestator-deployment.yaml`
- `ingress-orchestator.yaml`
- `ingress-dkms.yaml`
- `ingress-sdn.yaml`

## Runtime behavior

- AuthZ is exposed for `/login`, `/authorize`, and `/health`
- Orchestrator is exposed under `/orch/*`
- simulation services are exposed under `/api/sim/<simulation_id>/*`
- ingress subrequests use AuthZ to validate access and return user headers

## Key inputs

Typical deployment-time substitutions include:

- ingress class and host
- AuthZ subrequest URL
- forwarded auth headers

## Related docs

- [../README.md](../README.md)
- [../../authz/README.md](../../authz/README.md)
- [../../DKMS/README.md](../../DKMS/README.md)
- [../../SDN/README.md](../../SDN/README.md)
- [../../../../docs/OPERATIONS.md](../../../../docs/OPERATIONS.md)
- [README.k8s.ingress.md](README.k8s.ingress.md)

## Known constraints

- Path matching depends on ingress-nginx behavior compatible with
  `ImplementationSpecific`.
- Auth behavior depends on ingress snippet and annotation policy.
