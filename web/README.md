# DKMS Studio Web

## Purpose

`web/` contains the Next.js application used as the browser-facing management
UI for simulations.

It runs as a remote BFF: authentication and simulation management are delegated
to AuthZ and the Orchestrator API rather than implemented directly in the
browser.

## What it contains

- Next.js App Router application with React and TypeScript
- internal BFF routes under `/web/api/*`
- visual topology editor built around React Flow
- simulation CRUD and SAE management UI

## Key inputs

Typical environment variables:

```env
ORCH_BASE_URL="http://localhost:8080"
AUTHZ_BASE_URL="http://localhost:8081"
ORCH_TOKEN_COOKIE_NAME="dkms_orch_token"
ORCH_USER_COOKIE_NAME="dkms_orch_user"
ORCH_REQUEST_TIMEOUT_MS="15000"
ORCH_COOKIE_SECURE="false"
WEB_RUNTIME_BASE_URL="https://api.example.com"
```

Notes:

- `ORCH_BASE_URL` points to the Orchestrator management API
- `AUTHZ_BASE_URL` points to AuthZ; if omitted, the app can derive it from
  `ORCH_BASE_URL`
- `ORCH_COOKIE_SECURE` should be `true` for HTTPS deployments
- `WEB_RUNTIME_BASE_URL` is used to build runtime DKMS URLs and SAE bundle
  download references

## Local development

```bash
cd web
cp .env.example .env.local
npm install
npm run dev
```

Main routes:

- `http://localhost:3000/web/login`
- `http://localhost:3000/web/register`
- `http://localhost:3000/web/simulations`

## Runtime behavior

- authentication is proxied through `/web/api/auth/*`
- session tokens are stored in `HttpOnly` cookies
- `middleware.ts` protects `/simulations` routes
- the topology editor can import and export the canonical topology document
- simulation edits are saved remotely through Orchestrator web endpoints
- running simulations expose SAE lifecycle actions such as issue, revoke,
  delete, and bundle download

## Related docs

- [docker/README.md](docker/README.md)
- [k8s/README.md](k8s/README.md)
- [../orchestrator/readme.api.orchestator.md](../orchestrator/readme.api.orchestator.md)

## Known constraints

- The editor intentionally ignores domain fields that the backend does not
  currently expose or persist.
- The web layer and script tooling do not preserve exactly the same optional
  topology fields in every round-trip.
