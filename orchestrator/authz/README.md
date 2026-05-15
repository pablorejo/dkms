# AuthZ Module

## Purpose

This module implements the authentication and authorization service used by the
management plane and ingress-protected APIs.

## What it contains

- `main.py`: login, registration, token validation, and authorization checks
- persistence access built through `build_uow_from_env`

## Runtime behavior

Main endpoints:

- `GET /health`
- `POST /login`
- `POST /register`
- `GET /me`
- `GET /authorize`
- `HEAD /authorize`

Key behaviors:

- issue or validate JWTs
- return user profile information for web sessions
- validate simulation ownership for ingress-subrequested routes
- return headers such as `X-User-Id` and `X-Simulation-Id` when access is
  granted

## Key inputs

Important environment variables include:

- `JWT_SECRET`, `JWT_PUBLIC_KEY`, `JWT_PRIVATE_KEY`
- `JWT_ALGORITHMS`, `JWT_AUDIENCE`, `JWT_ISSUER`, `JWT_USER_CLAIM`,
  `JWT_LEEWAY`
- `AUTHZ_TOKEN_TTL`
- `AUTHZ_SIM_ID_REGEX`, `AUTHZ_SIM_ID_HEADER`
- `AUTHZ_USER_ONLY_PATHS`, `AUTHZ_USER_ONLY_METHODS`
- `AUTHZ_REQUIRE_ACTIVE`
- `AUTHZ_HOST`, `AUTHZ_PORT`, `AUTHZ_BIND_PORT`
- `PERSISTENCE_BACKEND`, `DB_URL`

## Related docs

- [../k8s/README.md](../k8s/README.md)
- [../k8s/ingress/README.md](../k8s/ingress/README.md)
- [../persistence/README.md](../persistence/README.md)
- [README.authz.md](README.authz.md)

## Known constraints

- AuthZ depends on the configured persistence backend to resolve users and
  simulations.
- Authorization rules assume ingress path and header conventions that match the
  deployment manifests.
