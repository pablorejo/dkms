# web

The Next.js management UI from the Python project is reused **as-is**.
This directory exists only as a pointer.

## Where the real code lives

The frontend source is at `../dkms/web/` (relative to this repo, assuming
both repos are cloned next to each other).

## How it talks to the Rust backend

The web app expects to hit two HTTP endpoints:

| Path prefix | Module | Notes |
|-------------|--------|-------|
| `/api/sdn/*`  | sdn   | Read-only topology + admin endpoints |
| `/api/dkms/*` | dkms  | ETSI surface (proxied via auth) |

Set `NEXT_PUBLIC_API_BASE` (or the equivalent variable for whatever
auth/orchestrator proxy you have in front) to your SDN HTTP port (8081)
and the DKMS HTTP port (8080).

## Why we didn't port it to Rust

It's a Next.js app — the language choice on the server side is invisible
to it. Porting it would just be re-writing a UI in a less-suited stack.
The Rust side exposes the same routes the Python side did (plus typed
gRPC for orchestrators), so the web doesn't need any structural change.

## Migrating later (optional)

If you want a Rust-side web (e.g. a leptos or axum-rendered admin
console), the cleanest landing spot would be a new `web/` crate in this
workspace that hits the existing SDN / DKMS HTTP and gRPC surfaces — not
a rewrite from scratch.
