# dkms_rust

Distributed Key Management System (DKMS) digital twin platform for QKD and PQC
workflows — rewritten in Rust, modularized so each component is independently
deployable.

This is the Rust reimplementation of [pabloprejo/dkms](../dkms) (Python). The web
frontend is reused unchanged.

## Modules

Each module is its own binary crate. They communicate over **gRPC (tonic)** for
the control plane, and over a **binary TCP protocol** for the QKC↔QKC hot path
(per-link key transport). See [docs/ipc.md](docs/ipc.md) for the wire formats.

| Crate       | Role |
|-------------|------|
| [`qkc`](qkc/)         | Quantum Key Channel — encrypted per-link key transport, token bucket admission, retry windows |
| [`orr`](orr/)         | Onion Routing Router — per-hop PQC handshakes and relay forwarding |
| [`sdn`](sdn/)         | Software Defined Network — in-memory topology graph, route computation (MCF), link admission, metrics sync |
| [`dkms`](dkms/)       | Distributed Key Management Service — ETSI 014/020 endpoints for SAEs, buffered delivery, per-SAE rate limiting, round-robin scheduling |
| [`quditto`](quditto/) | Simulated QKD link endpoint (replaces `simple_quditto`) |
| [`common`](common/)   | Shared library: protobuf-generated types, IPC helpers, config, logging, crypto, IDs |
| [`proto/`](proto/)    | Protobuf service definitions (input to `tonic-build`) |

The runtime request path is unchanged from the Python version:

```
SAE → DKMS → ORR → QKC → (peer node) → ORR → DKMS → SAE
                ↕
              SDN (routing and link binding)
```

## Building

```bash
# Whole workspace
cargo build --release

# Just one module
cargo build --release -p qkc
cargo build --release -p sdn
cargo build --release -p dkms
cargo build --release -p orr
cargo build --release -p quditto
```

Binaries land in `target/release/{qkc,orr,sdn,dkms,quditto}`.

## Running locally

```bash
cp .env.example .env
# edit .env if you need to override ports/paths

# Run a single module
./scripts/run-sdn.sh
./scripts/run-qkc.sh
./scripts/run-orr.sh
./scripts/run-dkms.sh
./scripts/run-quditto.sh

# Or everything at once with docker-compose
docker compose -f docker/docker-compose.yml up
```

## Web

The Next.js web frontend from the Python project is reused unchanged. See
[`web/README.md`](web/README.md) for the pointer.

## Layout

```
.
├── Cargo.toml          # workspace
├── proto/              # .proto schemas
├── common/             # shared library crate
├── qkc/                # binary crate
├── orr/                # binary crate
├── sdn/                # binary crate
├── dkms/               # binary crate
├── quditto/            # binary crate
├── web/                # pointer to Next.js web (unchanged)
├── docker/             # compose files (full + per-module)
├── k8s/                # shared k8s manifests
├── scripts/            # build/run helpers
└── docs/               # architecture, IPC, deployment
```

## License

LGPL-3.0-or-later. See [LICENSE](LICENSE).
