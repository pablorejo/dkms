# qkc

**Quantum Key Channel.** Per-link encrypted key transport between adjacent
nodes. Owns:

- A **KME** (Key Management Entity) with one key buffer per peer.
- A **token bucket** per directed link for admission control.
- A **routing resolver** populated by SDN.
- Two listeners:
  - **gRPC** on `QKC_GRPC_ADDR` (default `:50051`) — control plane.
  - **Binary TCP** on `QKC_TCP_BIND` (default `:7001`) — hot path.

## Build / run

```bash
cargo build --release -p qkc
RUST_LOG=info CONFIG_DIR=./qkc/config ./target/release/qkc
# or:
./scripts/run-qkc.sh
```

## Configuration

See `config/default.toml`. Per-deployment overrides via `config/local.toml`
or `QKC_*` env vars.

## Wire formats

- gRPC schema: [`/proto/qkc.proto`](../proto/qkc.proto)
- Binary TCP wire: documented in
  [`common/src/ipc/binary_tcp.rs`](../common/src/ipc/binary_tcp.rs) and
  [`docs/ipc.md`](../docs/ipc.md).

## Dependencies on other modules

- Pulls fresh key material from a local **quditto** (`QKC_QUDITTO_URL`).
- Pushes capacity reports to **SDN** (`QKC_SDN_URL`).
- Receives bucket / routing updates from **SDN** via streaming RPCs.
