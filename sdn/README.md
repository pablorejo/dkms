# sdn

**Software Defined Network module.** Owns the in-memory topology graph,
computes routes (MCF / shortest-hops / min-latency / max-capacity), runs
link admission control, and streams topology updates to QKC, ORR and DKMS.

## Listeners

| Port  | Protocol | Purpose |
|-------|----------|---------|
| 50053 | gRPC     | Control plane (`SdnControl` service) |
| 8081  | HTTP     | Admin/read-only API for the web UI |
| 9102  | HTTP     | `/metrics` (Prometheus) |

## Topology JSON

Compatible with the Python `config/topology.json` from the old project.
Set `topology_file` in `config/default.toml` to seed at boot, or
`PUT /topology` over gRPC at runtime.

## Build / run

```bash
cargo build --release -p sdn
./scripts/run-sdn.sh
```
