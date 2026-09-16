# quditto — simulated QKD key server

`quditto` is an ETSI GS QKD 014 key server (a KME) with no quantum hardware
behind it. A background task fills a FIFO of random keys (256-bit by default)
at `R0 · 10^(−α·d/10)` keys/s, capped at `max_key_count`, and the standard
`/api/v1/keys/{SAE_id}/{status,enc_keys,dec_keys}` endpoints serve them. To a
QKC it is indistinguishable from a real KME: a `type: qkd` link points its
`kme_url` here and nothing else changes. One binary, configured by CLI flags
with `QUDITTO_*` environment fallbacks; there is no config file.

## What it is for, and what it is not

It exists so that `qkd` links can be exercised without hardware: tests, the
demos under `scripts/`, the local N-node mesh (`tests/local-mesh`) and the
60-cell CESGA campaign of 2026-09, which ran every link on one quditto per
edge. It is a simulator, not a product. **One quditto per link, not per
node**: real hardware has a KME at each end, here the two QKCs of a link
point at the *same* process — one calls `enc_keys`, the other retrieves the
same `key_ID`s with `dec_keys` — so both compete for one FIFO and there is no
per-direction rate. The keys are pseudo-random (`ChaCha20Rng` seeded once
from the OS). It is not hardened: the `SAE_id` in the path is ignored (one
pool for everybody), any client certificate from the network CA can pull
keys, and a key handed out by `enc_keys` and never collected by `dec_keys`
stays in memory. The gRPC service in `proto/quditto.proto` (`QudittoControl`)
is compiled with the other schemas but neither served nor called by anything.

## Endpoints

Routes in `src/server.rs`; bodies are ETSI-014 JSON (`key` in base64) using
the types of the [`etsi`](../etsi/README.md) crate.

| Method and path | What it does |
|---|---|
| `GET /healthz` | Liveness: `{"status":"ok"}`. |
| `GET /api/v1/keys/{SAE_id}/status` | `stored_key_count` = keys in the FIFO, `max_key_count` = `--max-buffer`, `key_size` = `--key-size-bits`, `max_key_per_request` = 128, `max_SAE_ID_count` = 1. `source_KME_ID`/`target_KME_ID` are `quditto`, `master_SAE_ID` is `quditto-master`, `slave_SAE_ID` echoes the path. |
| `GET /api/v1/keys/{SAE_id}/enc_keys?number=N&size=B` | Pops up to N fresh keys into a `delivered` map keyed by `key_ID`. `size` must equal the configured key size (400 otherwise). 503 when the FIFO is empty; fewer than N comes back as a partial batch with a warning, never as an error, because the keys have already left the FIFO. |
| `GET /api/v1/keys/{SAE_id}/dec_keys?key_ID=<uuid>` | Returns that delivered key and forgets it (one-shot). 404 if unknown or already consumed. |
| `POST /api/v1/keys/{SAE_id}/dec_keys` | Same for a batch, body `{"key_IDs":[{"key_ID":"<uuid>"},…]}`. 404 only if none is found; otherwise the subset. |

`enc_keys` and `dec_keys` also answer in the compact binary encoding of
`etsi::binary` when the client sends `Accept: application/octet-stream`. No
client in the repository asks for it: the QKC sends `Accept:
application/json` so that it works unchanged against a real KME.

## The rate model

The link produces key at a constant rate set by fibre attenuation
(`src/link.rs`, `link_rate_kps`), independent of how full the store is:

```
R(d) = R0 · 10^(−α·d/10)   keys/s
```

With the values used in the repository's tests, `R0 = 2000` keys/s, `α = 0.2`
dB/km, `d = 5` km, the factor is `10^(−0.1) = 0.794` and the link gives
**1588.7 keys/s**. The SDN sizes a `qkd` edge with the same formula from the
`r0/alpha/distance_km` that the two QKCs announce (`sdn/src/topology.rs`,
`quditto_capacity_keys_per_second`), so the same three numbers go in the
quditto and in both QKCs. Capacity is `R(d)`, never `R0`: read
[Theoretical rate — NOT R0](../docs/engineering-notes.md#theoretical-rate--not-r0)
before comparing a measurement with "the theoretical".

The minter (`src/service.rs`, `run_minter`) ticks every 100 ms and mints
`R/10` keys per tick when `R ≥ 10` keys/s, one key every `1/R` seconds below
that; fractional keys carry over, so the long-run rate is exact. The FIFO
holds `--max-buffer` keys (default 8192). When it is full, `--full-mode drop`
(default) mints and discards, counting `dropped`, like a KME that keeps
distilling; `--full-mode pause` stops the minter until there is room,
counting `paused`, with no catch-up burst on resume, like hardware that stops
distilling when its store is full. A consumer cannot tell them apart:
`stored_key_count` sits at `max_key_count` and that production is invisible.
For estimator tests, `--block-keys N` releases keys in blocks of N (a
staircase in `stored_key_count`, like real privacy-amplification output) and
`--rate-step SECONDS:FACTOR` (e.g. `120:0.5`) multiplies the rate by FACTOR
once, SECONDS after start.

## TLS

The `enc_keys`/`dec_keys` bodies carry the link keys (one-time-pad material), so the server is
**mTLS by default** (`--tls on`, `src/tls_server.rs`): HTTPS with the
workspace's post-quantum provider (X25519MLKEM768, ML-DSA certificates) and a
client certificate **required**, verified against `--tls-client-ca`. Without
`--tls-cert`, `--tls-key` and `--tls-client-ca` it refuses to start and says
what it needs. `docker/gen-certs.sh quditto <ip> ./certs` issues them under
the network CA (`net-ca.crt`) that also signs the QKC node certificates, which
is what a QKC presents when `kme_url` is `https://` (unless the link declares
a per-KME credential, `kme_cert`/`kme_key`/`kme_ca`). `--tls off`
(`QUDITTO_TLS=off`) serves plain HTTP and warns at boot: only when the QKCs
and the quditto share a host or a trusted network, with `kme_url: http://…`.

## Running it

```bash
cargo build --release -p quditto
QUDITTO_TLS=off ./scripts/run-quditto.sh --listen 127.0.0.1:20010 \
    --r0 2000 --alpha 0.2 --distance 5
curl -s http://127.0.0.1:20010/api/v1/keys/1/status
```

| Flag | Env | Default | Meaning |
|---|---|---|---|
| `--listen` | `QUDITTO_LISTEN` | `0.0.0.0:8080` (binary default) | Bind address; a rendered `node.yml` sets port 20010. |
| `--r0` | `QUDITTO_R0` | `1000` (binary default) | Rate at zero distance, keys/s. Note the SDN's own fallback for an undeclared edge is 2000 and the mesh/campaign use 2000; a rendered `node.yml` makes `r0` required, so the 1000 only applies to a bare run. |
| `--alpha` | `QUDITTO_ALPHA` | `0.2` | Fibre attenuation, dB/km. |
| `--distance` | `QUDITTO_DISTANCE` | `0` | Link length, km. |
| `--max-buffer` | `QUDITTO_MAX_BUFFER` | `8192` | FIFO capacity in keys (`max_key_count`). |
| `--key-size-bits` | `QUDITTO_KEY_SIZE_BITS` | `256` | Key size, positive multiple of 8. |
| `--full-mode` | `QUDITTO_FULL_MODE` | `drop` | `drop` or `pause`. |
| `--block-keys` | `QUDITTO_BLOCK_KEYS` | `0` | Block delivery; 0 = continuous. |
| `--rate-step` | `QUDITTO_RATE_STEP` | none | `SECONDS:FACTOR`, one step. |
| `--tls` | `QUDITTO_TLS` | `on` | `on` (mTLS) or `off` (plaintext). |
| `--tls-cert`, `--tls-key`, `--tls-client-ca` | `QUDITTO_TLS_CERT`, `QUDITTO_TLS_KEY`, `QUDITTO_TLS_CLIENT_CA` | none | Server certificate and key (PEM) and the client CA. Required with `--tls on`. |

**Container.** `docker/compose/quditto.yml` runs the image from
`docker/Dockerfile` with `network_mode: host`, `./node.yml` at
`/config/node.yml` and `./certs` at `/config/certs`. The entrypoint renders
`node.yml` (template `docker/examples/node.quditto.yml`: `r0` required, HTTP
port 20010, `tls: true`, `cert_name: quditto`) into `QUDITTO_*` exports
(`docker/render_config.py`, `render_quditto`); `full_mode`, `block_keys` and
`rate_step` have no `node.yml` key and go as `QUDITTO_*` variables in the
container environment. Recipe and the `curl` check with a client certificate:
[docker/README.md](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator).

## Pointing a QKC link at it

In the `node.yml` of **both** QKCs of the link (the SDN creates `pqc` links
from one side's announcement, but it cannot invent a `kme_url`):

```yaml
links:
  - { neighbor_id: 2, neighbor_addr: "10.0.0.12", type: qkd,
      kme_url: "10.0.0.50:20010", r0: 2000, alpha: 0.2, distance_km: 5 }
```

The renderer prepends `https://` when no scheme is given and writes a
`[[links]]` entry with `link_type = "qkd"` and `quditto_url` in `qkc.toml`
(`qkc/src/config.rs`). The QKC does not use `r0`, `alpha` or `distance_km`; it
forwards them to the SDN, which sizes the edge with them until a measurement
replaces them. `key_size_bits` (256 in the examples) should match on all
three; the QKC (`qkc/src/kme.rs`) probes `/status` before its first
`enc_keys` and adopts the announced `max_key_per_request` and `key_size`
(warning if that one differs). The path `SAE_id` is the QKC's `qkc_id` unless
the link sets `sae_id` in `qkc.toml` (no `node.yml` key); quditto ignores it.

## The QKC's in-situ rate estimator and quditto

`r0/alpha/distance_km` are simulator parameters that a real deployment does
not know, so the QKC also **measures** the link rate from `/status` plus the
deliveries it counts, and the SDN lets the measurement override the formula
when the two drift by 5 % or more. How the estimate is built (conservation
with censoring, the 30 s mean, the `floor`, the banking) is in
[qkc/README.md § The in-situ rate estimator](../qkc/README.md#the-in-situ-rate-estimator)
and, with the measurements behind each rule, in
[QKD rate estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01).
What quditto contributes is the part the estimator cannot see: a KME whose
stock sits at `max_key_count` hides its production, in `drop` and `pause`
alike, which is why the censoring exists. On quditto the prior and the
measurement agree, so the override usually stays dormant; on real hardware
the formula is only the cold-start prior. `--full-mode pause`, `--block-keys`
and `--rate-step` exist to validate the estimator:
`scripts/test-rate-estimator.sh` runs two QKCs against one quditto with no
data traffic (0.2–1.5 % error; a −50 % step tracked in about 12 s).

## What to look at

`tracing` on stdout, level from `RUST_LOG` (default `info`): `quditto
starting` (whole config, `tls=true|false`), then `quditto: minter started`
with `rate_kps`, the computed `R(d)`; every 30 s `quditto.stats` with
`fresh_available` (FIFO fill, what `/status` reports), `delivered_pending`
(handed out, not yet collected), cumulative `generated_total`,
`dropped_total`, `delivered_enc_total`, `delivered_dec_total`, and the last
window's `gen_kps`, `drop_kps`, `paused_kps`, `enc_serve_kps`, `dec_serve_kps`.
`fresh_available` pinned at 0: the consumers want more than `R(d)`;
`drop_kps` or `paused_kps` at `R(d)` with `gen_kps` near 0: they are idle or
full (`generated` only counts keys that entered the FIFO);
`generated_total` not advancing with nothing dropped or paused either: the
minter stalled. `… returning partial
batch` at `warn` is normal at low rates with large requests; `tls handshake
failed` is a client without a valid certificate or one speaking plain HTTP.
Downstream, the QKC logs `keystore.levels` per peer with `rate` and `rate_q`
(`measured`, `floor`, `unavailable`), and the SDN's `GET /links` shows
`capacity_keys_per_second` (the effective value: the formula until a
measurement overrides it) next to `measured_keys_per_s` and
`r0_keys_per_second`.

## Further reading

- [docs/architecture.md](../docs/architecture.md), [docs/ipc.md](../docs/ipc.md) — where the KME sits among the five modules and how they talk.
- [docs/auto-configuration.md](../docs/auto-configuration.md) — why quditto announces nothing and why `qkd` links stay in the `node.yml` of both QKCs.
- [qkc/README.md](../qkc/README.md) — the consumer: links, key store, estimator.
- [etsi/README.md](../etsi/README.md) — the ETSI-014 types and the binary encoding.
- [docker/README.md — QKD links without hardware](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator) — deployment recipe, certificates, the QKC side of `kme_url`.
- [docs/engineering-notes.md](../docs/engineering-notes.md) — [Theoretical rate](../docs/engineering-notes.md#theoretical-rate--not-r0), [Defaults](../docs/engineering-notes.md#defaults), [QKD rate estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01).
- [tests/local-mesh/README.md — Simulated QKD links](../tests/local-mesh/README.md#simulated-qkd-links) — one quditto per edge with `DKMS_MESH_LINK_TYPE=qkd`.
- [docs/results/campaign-2026-09.md — The QKD link model](../docs/results/campaign-2026-09.md#the-qkd-link-model) — 60 cells, every link on a quditto.
