# common — the shared library

Everything that crosses a crate boundary lives here; each module's own
business logic does not. The boundary *between* modules is the protobuf
schema in [`proto/`](../proto/), compiled into `common::proto`: internal
Rust types are never re-exported across crates, so a module talks to
another only through generated messages, an HTTP body or a `wire` frame
(the rule is in
[engineering-notes.md](../docs/engineering-notes.md#things-to-not-do)).

## What is in here, by concern

| Concern | Modules | What they provide |
|---------|---------|-------------------|
| Plumbing | `config`, `logging`, `metrics`, `ids`, `error`, `log_throttle`, `net` | layered config with `SecretString`; the `tracing` bootstrap (`RUST_LOG`, `LOG_FORMAT=json`); a per-module Prometheus registry and exporter; validated id newtypes (`NodeId`, `SaeId`, `KeyId`, `LinkId`: `try_new` / `is_valid_id` for anything that arrives from the network: 1 to 64 bytes of `[A-Za-z0-9._:@+-]`); `CommonError`; `nth_is_loud` (log the first event, then powers of two); `bind_reuse_addr` |
| Transport | `ipc`, `http` | `ipc::grpc::DialOpts` and `connect` (a gRPC dial baseline with keepalive and lazy connect that no module uses yet: the DKMS and the ORR build their tonic endpoints directly); `ipc::binary_tcp` re-exporting the [`wire`](../wire/README.md) crate (no current call site); `http::announcer_client` / `mtls_client`, the reqwest client every announce loop and the SDN's forwarding push share, mTLS-aware from the URL scheme |
| Identity and channels | `tls`, `tls_pqc`, `cert_identity` | rustls server configs for the axum listeners (and client configs, used by the tests); tonic and reqwest take the same provider from the process default that `ensure_process_default` installs at boot; the one crypto provider (ML-DSA-65 certificates, TLS 1.3, `X25519MLKEM768` as the only key-exchange group, a boot self-check that aborts on anything else); the node id read from the SAN URI `dkms://<node_id>` of a verified certificate |
| Primitives | `crypto::{pqc, pqc_sign, aead, frame_mac, link_mac, otp}` | ML-KEM (FIPS 203), ML-DSA-65 (FIPS 204), AES-256-GCM in detached mode (tag outside the payload, for the ORR onion and the DKMS e2e seal), the per-frame link MAC with session, counter and anti-replay window, the legacy HMAC of the PSK handshake, the OTP; plus `ct_eq` |
| Policy | `security`, `hardening` | `KeyGrade` (`qkd` / `pqc`), `SecurityLevel` (`strict_qkd`, `qkd_prefer`, `no_worry`) and `resolve_grade`, the level-to-grade policy written in one place (no caller outside its own tests yet); `harden_process` (`mlockall`, no core dumps — the only `unsafe` in the crate, `#![deny(unsafe_code)]` elsewhere) |
| Test support | `test_support` | `mldsa_test_pki` (a throwaway ML-DSA PKI, `None` where openssl is too old) and `skip_or_fail`, so a test that cannot run says so instead of passing empty |

## Protobuf

[`build.rs`](build.rs) compiles every `.proto` under `proto/` with
`tonic_build` (server and client stubs) and re-runs when one changes. Each
package lands under the module named after its `package` line without the
`dkms.` prefix: `common::proto::common::v1`, `sdn::v1`, `orr::v1`,
`dkms::v1`, `qkc::v1`, `quditto::v1`. Which of those services are actually served, and which RPCs
answer `UNIMPLEMENTED` on purpose, is in [docs/ipc.md](../docs/ipc.md).

## Configuration

`common::config::load_config::<C>("<module>")` builds a module's `Config`
(defined in that module's `src/config.rs`) from three layers, later wins:

1. `config/default.toml` (in `CONFIG_DIR`, default `config`);
2. `config/local.toml`, optional and gitignored, for local overrides;
3. environment variables `MODULE__section__key` — prefix is the module name
   upper-cased, `__` separates nesting, values are parsed
   (`DKMS__buffer__capacity_per_peer=8192`).

An env override of one nested field **merges** into the file's section; the
other fields of that section survive (pinned by the test
`an_env_override_of_one_nested_field_merges_into_the_section`). The SDN, ORR
and DKMS load this way; the QKC reads the TOML named by `--config`, quditto
reads CLI flags with `QUDITTO_*` fallbacks. In a container,
[`docker/render_config.py`](../docker/render_config.py) writes the TOML from
`node.yml`.

Secrets in config (`link_psk`, `sign_secret_seed`) are `SecretString`: it
deserialises as the string it wraps and is read with `expose()`, but its
`Debug` prints `<redacted>` and it has neither `Display` nor `Serialize`.
The SDN, ORR, QKC and quditto log `info!(?cfg)` at boot (the DKMS logs only
its `node_id`) and `docker logs` is the diagnostic channel, so a derived
`Debug` would put every PSK in the container log.

## Further reading

- [docs/architecture.md § 5](../docs/architecture.md#5-the-layers) — which
  of these primitives protects what, at which position on the path.
- [docs/ipc.md](../docs/ipc.md) — the three planes, the gRPC services, the
  ports.
- [docs/SECURITY.md](../docs/SECURITY.md) — why the TLS provider is
  hybrid-only and the certificates are ML-DSA-65.
- [wire/README.md](../wire/README.md) — the binary frame the `ipc` module
  re-exports.
- [docs/engineering-notes.md](../docs/engineering-notes.md) — the config
  merge rule, the redaction rule and the other invariants, with dates.
- `make doc-open` — the crate's rustdoc, private items included.
