# Orchestrator API Contract

## Purpose

This document summarizes the HTTP contract implemented in
`code_dkms/src/k8s/api_orchestator.py`.

## Authentication model

- requests are expected to arrive through ingress with `X-User-Id` already set
- `X-Simulation-Id` may also be provided and is validated when relevant
- direct calls that bypass the expected ingress flow often fail with `401`

## Route groups

### Health

- `GET /orch/health`

### Simulation lifecycle

- `POST /orch/simulations`
- `GET /orch/simulations`
- `GET /orch/api/sim/{simulation_id}`
- `POST /orch/api/sim/{simulation_id}/run`
- `POST /orch/api/sim/{simulation_id}/stop`
- `DELETE /orch/api/sim/{simulation_id}`

### Web-facing simulation CRUD

- `GET /orch/web/simulations`
- `POST /orch/web/simulations`
- `GET /orch/web/simulations/{simulation_id}`
- `PATCH /orch/web/simulations/{simulation_id}`
- `DELETE /orch/web/simulations/{simulation_id}`
- `POST /orch/web/simulations/{simulation_id}/run`
- `POST /orch/web/simulations/{simulation_id}/stop`
- `GET /orch/web/simulations/{simulation_id}/runs`

### SAE administration

- `GET /orch/admin/saes`
- `POST /orch/admin/saes`
- `POST /orch/admin/saes/{sae_id}/csr`
- `POST /orch/admin/saes/{sae_id}/issue`
- `GET /orch/admin/saes/{sae_id}/bundle`
- `POST /orch/admin/saes/{sae_id}/revoke`
- `DELETE /orch/admin/saes/{sae_id}`

For some SAE operations keyed by `sae_id`, pass `simulation_id` as a query
parameter to disambiguate the target simulation.

Compatibility aliases are also exposed for simulation-scoped SDN consumers:

- `GET /orch/api/sim/{simulation_id}/sdn/sae/{sae_id}/binding`
- `GET /orch/api/sim/{simulation_id}/sdn/resolve-sae?sae_id=...`

## Behavioral notes

- `POST /orch/simulations` binds the created simulation to the authenticated
  user from `X-User-Id`
- run, stop, patch, and delete paths enforce ownership checks
- `/orch/web/*` persists editor-focused topology JSON and exposes web DTOs
- `/orch/web/simulations/{id}/runs` stores run history entries
- SAE admin routes handle issue, bundle download, revoke, and delete flows
- when `POST /orch/admin/saes` targets a running simulation, orchestrator now
  keeps the simulation-scoped SDN binding in sync both for newly created SAEs
  and when an existing SAE record is rehydrated from the database
- the SDN compatibility binding payload now mirrors the enriched
  `dkms_endpoint.id` field exposed by the SDN service when the DKMS host id is
  known

## Related docs

- [README.md](README.md)
- [ingress/README.md](ingress/README.md)
- [../authz/README.md](../authz/README.md)
- [../../../docs/OPERATIONS.md](../../../docs/OPERATIONS.md)

## Known constraints

- Access control depends on the trusted ingress-auth path.
- Persistence must be configured through `PERSISTENCE_BACKEND` and `DB_URL`.
