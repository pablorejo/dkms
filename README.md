# dkms_rust

Distributed Key Management System (DKMS) for QKD and post-quantum (PQC)
key distribution, written in Rust. Each institution runs one node made of
independent modules; the network topology is discovered from what the nodes
announce, not configured centrally. Every plane between modules is
authenticated and, where it carries key material, sealed end to end.

It is the Rust reimplementation of an earlier Python prototype, reorganised so
that every component is separately deployable.

## Modules

Each runtime module is its own binary crate. The control plane speaks
**gRPC (tonic)** under mutual TLS; the QKC↔QKC hot path uses a **binary TCP
protocol** (see [docs/ipc.md](docs/ipc.md)).

| Crate | Role |
|-------|------|
| [`qkc`](qkc/) | Quantum Key Channel: per-link key transport (QKD via ETSI 014 KMEs, or PQC ML-KEM), per-frame authentication, multipath forwarding, in-situ QKD rate estimation |
| [`orr`](orr/) | Onion Routing Router: relays key material between nodes, optional onion path privacy on top of the end-to-end seal |
| [`sdn`](sdn/) | Network controller: topology inferred from module announcements, routing tables, rate allocation (proportional fairness by default) |
| [`dkms`](dkms/) | Key Management Service facing the SAEs: ETSI GS QKD 014/020 endpoints, RAM-only key buffers, end-to-end sealing of transport material |
| [`quditto`](quditto/) | Simulated QKD link (an ETSI 014 KME with a configurable rate model) for tests and demos |
| [`etsi`](etsi/) | ETSI GS QKD 014/020 message types and validation |
| [`wire`](wire/) | Binary TCP wire format shared by QKC and ORR |
| [`common`](common/) | Shared library: protobuf-generated types, config loading, TLS (hybrid X25519+ML-KEM, ML-DSA certificates), crypto helpers, IDs, logging |
| [`proto/`](proto/) | Protobuf service definitions (compiled by `common/build.rs`) |
| [`tests/loadgen`](tests/loadgen/) | `sae_load`: an SAE load client over mTLS, used by the test harnesses |

Request path:

```
SAE → DKMS → ORR → QKC → (peer node) → QKC → ORR → DKMS → SAE
                ↕
              SDN (topology, routing, rates)
```

## Building

Requires Rust 1.88 (pinned in `rust-toolchain.toml`), `protobuf-compiler`,
`cmake` and `clang`/`libclang`.

```bash
cargo build --release              # whole workspace
cargo build --release -p qkc       # one module
make check                         # fmt + clippy -D warnings + renderer tests + tests (no skips)
make deny                          # cargo-deny: advisories and licences
```

Binaries land in `target/release/{qkc,orr,sdn,dkms,quditto,sae_load}`.

## Running

- **Single module, locally**: `scripts/run-<module>.sh` (config from
  `<module>/config/default.toml`, overridable with `local.toml` and
  `MODULE__section__key` environment variables).
- **Local multi-node demos**: [`scripts/demo-3qkc/`](scripts/demo-3qkc/) and
  `scripts/demo-star/` (several nodes on one machine), and
  [`scripts/demo-idq/`](scripts/demo-idq/) (two QKCs over real ID Quantique KMEs).
- **Deployment**: one container image per module, one `node.yml` per
  institution, `docker compose up`. Start with
  [docker/README.md](docker/README.md) and
  [docker/examples/quick_start.md](docker/examples/quick_start.md).
  `make images` builds the images.

## Tests

`cargo test --workspace` covers the unit and integration tests. Beyond that:

- [`tests/local-mesh/`](tests/local-mesh/): an N-node mesh on one machine
  (used for the scaling campaigns, up to N=100).
- [`tests/testbed/`](tests/testbed/): the multi-host test plan (restarts,
  hot add of a node, load, idle reconnects).

Load and saturation tests should run inside a memory-bounded cgroup; see the
notes below.

## Documentation

- [docs/architecture.md](docs/architecture.md): modules and data flow.
- [docs/ipc.md](docs/ipc.md): gRPC schemas and the binary wire format.
- [docs/deployment.md](docs/deployment.md) and [docker/README.md](docker/README.md): running a node.
- [docs/SECURITY.md](docs/SECURITY.md): trust model and the hardening phases.
- [docs/engineering-notes.md](docs/engineering-notes.md): design invariants,
  defaults and the gotchas measured along the way. Read it before touching
  the topology, rate or key-material paths.

## License

Apache-2.0. Copyright 2026 Pablo Pío Rejo Iglesias. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
