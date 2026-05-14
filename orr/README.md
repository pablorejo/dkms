# orr

**Onion Routing Router.** Per-hop PQC handshakes + relay forwarding for
multi-hop key paths.

## Build / run

```bash
cargo build --release -p orr
./scripts/run-orr.sh
```

## Wire format

gRPC schema: [`/proto/orr.proto`](../proto/orr.proto).

## Dependencies

- Talks to **SDN** for topology + path computation.
- Talks to peer **ORR**s over gRPC for hop-by-hop relay.
- The actual link-level key transport is owned by **QKC**.
