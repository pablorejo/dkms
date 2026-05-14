# quditto

Simulated QKD link endpoint. Replaces `simple_quditto` from the Python
project. Pretends to be a physical QKD device pumping fresh key material
into a local buffer at a rate `r(t) = r0 * exp(-alpha * fill_fraction)`.

QKC pulls keys from quditto via `QudittoControl::ReadKeys`.

## Build / run

```bash
cargo build --release -p quditto
./scripts/run-quditto.sh
```

## Config

See `config/default.toml`. The rate model parameters mirror the Python
values: `QUDITTO_RATE_R0`, `QUDITTO_RATE_ALPHA`, `QUDITTO_MAX_BUFFER_BYTES`.
