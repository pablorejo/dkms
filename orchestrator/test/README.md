# K8s Test Utilities

## Purpose

This directory contains helper scripts for quick validation of ingress-routed
DKMS endpoints.

## What it contains

- `request_dkms_ingress.py`: perform a request against
  `/api/sim/<simulation_id>/dkms/<dkms_id>/...` using either bearer auth or
  mTLS

## Runtime behavior

The helper script can:

- log in through AuthZ and call a runtime route with a bearer token
- call the runtime route directly with client certificates in mTLS mode
- print URL, status, headers, and response body for smoke debugging

## Key inputs

Useful CLI flags include:

- `--base-url`
- `--runtime-base-url`
- `--auth-mode`
- `--sim-id`
- `--dkms-id`
- `--path`
- `--username`, `--password`
- `--mtls-cert`, `--mtls-key`, `--mtls-ca`

## Related docs

- [../ingress/README.md](../ingress/README.md)
- [../../DKMS/README.md](../../DKMS/README.md)
- [../../../../docs/TESTING.md](../../../../docs/TESTING.md)
- [README.k8s.test.md](README.k8s.test.md)

## Known constraints

- These utilities require a reachable ingress host.
- Bearer mode only validates the management-auth path; many runtime deployments
  still require mTLS for final verification.
