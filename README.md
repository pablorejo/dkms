# dkms_rust

Distributed Key Management System (DKMS) for QKD and post-quantum (PQC)
key distribution, written in Rust. Each institution runs one node made of
independent modules; the network topology is discovered from what the nodes
announce, not configured centrally. Every plane between modules is
authenticated and, where it carries key material, sealed end to end.

It is the Rust reimplementation of an earlier Python prototype, reorganised so
that every component is separately deployable.

## Modules

Each runtime module is its own binary crate. Modules talk over three
planes: **gRPC (tonic)** control under mutual TLS, **HTTP** (announcements,
admin, ETSI GS QKD 014/020) and the **binary TCP wire** of the QKC↔QKC hot
path (see [docs/ipc.md](docs/ipc.md)).

| Crate | Role |
|-------|------|
| [`qkc`](qkc/) | Quantum Key Channel: per-link key transport (QKD via ETSI 014 KMEs, or PQC ML-KEM), per-frame authentication, multipath forwarding, in-situ QKD rate estimation |
| [`orr`](orr/) | Onion Routing Router: relays key material between nodes, optional onion path privacy on top of the end-to-end seal |
| [`sdn`](sdn/) | Network controller: topology inferred from module announcements, forwarding tables (WCMP), rate allocation (proportional fairness by default) |
| [`dkms`](dkms/) | Key Management Service facing the SAEs: ETSI GS QKD 014/020 endpoints, RAM-only key buffers, end-to-end seal on every transport key |
| [`quditto`](quditto/) | Simulated QKD link (an ETSI 014 KME with a configurable rate model) for tests and demos |
| [`etsi`](etsi/) | ETSI GS QKD 014/020 message types and validation |
| [`wire`](wire/) | Binary TCP wire format shared by QKC and ORR |
| [`common`](common/) | Shared library: protobuf-generated types, config loading, TLS (hybrid X25519+ML-KEM, ML-DSA certificates), crypto helpers, IDs, logging |
| [`proto/`](proto/) | Protobuf service definitions (compiled by `common/build.rs`) |
| [`tests/loadgen`](tests/loadgen/) | `sae_load`: an SAE load client over mTLS, used by the test harnesses |

Transport-key path (the buffers that fill in the background):

```
DKMS → ORR → QKC → (peer node) → QKC → ORR → DKMS
                ↕
              SDN (topology, routing, rates)
```

A SAE request is served DKMS → DKMS directly over ETSI-020, wrapped with
one of those transport keys; it never crosses the ORR or the QKCs.

## Building

Requires Rust 1.88 (pinned in `rust-toolchain.toml`), `protobuf-compiler`,
`cmake` and `clang`/`libclang`.

```bash
cargo build --release              # whole workspace
cargo build --release -p qkc       # one module
make check                         # fmt + clippy -D warnings + rustdoc + Markdown links + renderer tests + tests (no skips)
make deny                          # cargo-deny: advisories and licences
make doc-open                      # API reference (rustdoc, private items included) in the browser
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

The map is [docs/README.md](docs/README.md). The short version:

- **Run a node**: [docs/deployment.md](docs/deployment.md), then
  [docker/examples/quick_start.md](docker/examples/quick_start.md) for the
  commands and [docker/README.md](docker/README.md) for the full procedure.
- **Understand the design**: [docs/architecture.md](docs/architecture.md)
  (what a node is and how a key travels end to end),
  [docs/auto-configuration.md](docs/auto-configuration.md) (how the topology
  builds itself), [docs/ipc.md](docs/ipc.md) (every RPC, route and frame),
  one README per module (`qkc/`, `orr/`, `dkms/`, `sdn/`, `quditto/`),
  [docs/SECURITY.md](docs/SECURITY.md) (trust model, Spanish) and
  [docs/engineering-notes.md](docs/engineering-notes.md) (invariants, defaults
  and the gotchas measured along the way).
- **See it measured**: [docs/results/campaign-2026-09.md](docs/results/campaign-2026-09.md),
  the CESGA campaign at N = 10 to 100 with every security default on.
- **Change the code**: `make doc-open` for the rustdoc (private items
  included), `make check` for the gate CI runs.

## License

Apache-2.0. Copyright 2026 Pablo Pío Rejo Iglesias. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

Developed at atlanTTic (Universidade de Vigo) within the RETECH programme;
authors, supervisors and prior work are credited in [NOTICE](NOTICE).
