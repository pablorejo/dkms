# Architecture

dkms_rust is a five-binary distributed system. Each binary owns a single
responsibility, holds its own state, and exposes a typed RPC surface so
the others can call it without sharing in-process types.

## Modules

```
                 ┌────────────────┐
        ETSI 014 │     DKMS       │ gRPC (orchestrator)
        ─────────│ (axum + tonic) │──────────────────────
   (SAE client)  └───┬────┬───────┘
                    │    │
            gRPC  ┌─▼─┐  │ gRPC          ┌───────────────┐
        ──────────│ORR│  └────► SDN ◄────│  CapacityRpt   │
                  └─┬─┘         ▲        │    (QKC →)     │
                    │           │        └───────────────┘
                    │ gRPC      │ streaming
                    │           │ topology events
                    │     ┌─────┴─────┐
                    │     │   QKC     │
                    │     │ tonic +   │
                    └────►│ binary    │
                          │ TCP wire  │
                          └─────┬─────┘
                                │ ReadKeys (gRPC)
                                ▼
                         ┌────────────┐
                         │  quditto   │
                         │ (sim QKD)  │
                         └────────────┘
```

## Request path (SAE → SAE, multi-hop)

1. SAE calls `GET /api/v1/keys/{slave}/enc_keys` on its local **DKMS**.
2. DKMS asks **SDN** for a path (`ComputePath`).
3. DKMS asks **ORR** to open an onion circuit along that path
   (`OpenCircuit`).
4. DKMS reserves keys on the local **QKC** (`Reserve`).
5. The encrypted frame is pushed to the next-hop QKC over the **binary
   TCP wire** — fastest path because there's no gRPC framing, no JSON, no
   base64.
6. At each intermediate hop, the local ORR peels its onion layer, the
   local QKC re-encrypts with that hop's keys, and the frame goes out the
   next link.
7. At the terminal hop, DKMS returns the key bytes to the SAE.

## Why two transports?

We measured the Python prototype and saw ~35-45% byte overhead on the
QKC-to-QKC path from HTTP/JSON+base64. The hot path is the only place
where it matters, so we keep gRPC everywhere else (typed, easy to evolve)
and use a small custom binary TCP wire just for QKC↔QKC.

See [`ipc.md`](ipc.md) for transport details.

## State ownership

| State                            | Owner   | Persistence |
|----------------------------------|---------|-------------|
| Topology graph                   | SDN     | in-memory   |
| Routing table (next-hop per dst) | QKC     | populated by SDN |
| Per-link key buffers             | QKC     | in-memory   |
| Per-pair key buffers (SAE)       | DKMS    | in-memory   |
| Circuit table                    | ORR     | in-memory   |
| SAE registrations + buckets      | DKMS    | in-memory   |
| Topology subscribers             | SDN     | in-memory   |

No persistent storage today — same posture as the Python project.

## Independent deployability

Every module is its own binary crate (`cargo build -p <name>`) with its
own Dockerfile and `k8s/` manifests. The only inter-crate dependency is
on `common/`, which is consumed as a workspace member, not a separate
runtime artifact.

A minimal deployment can be just `quditto + qkc` (for a single QKD-only
link), `sdn + dkms` (for an orchestrated key fan-out), or any subset.
