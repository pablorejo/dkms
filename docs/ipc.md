# IPC: the three planes

How the modules talk to each other, over which transport, on which port,
and with what identity. Three planes and no fourth — but only two transports
of our own: HTTP is a plane because its protocols are HTTP (ETSI 014/020,
the KME, an operator with `curl`), not a third transport
([section 5](#5-the-rule)):

| Plane | Transport | Carries | Where |
|-------|-----------|---------|-------|
| Control | gRPC (tonic), schemas in [`proto/`](../proto/) | SAE resolution, ORR bootstrap and rotation, DKMS to ORR send/receive, operator RPCs | [section 1](#1-grpc-the-control-plane) |
| Admin and ETSI | HTTP (axum), mTLS | announcements to the SDN, forwarding pushes, ETSI GS QKD 014 and 020, the quditto KME | [section 2](#2-http) |
| Data | Binary TCP, the [`wire`](../wire/README.md) crate | every key-bearing frame between QKCs and between an ORR and its QKC | [section 3](#3-binary-tcp-the-wire) |

Terms (node, SDN, peer, link, announcement, …) are those of the
[glossary of architecture.md](architecture.md#10-glossary). The flows that
ride on these planes are in [architecture.md](architecture.md); the trust
roots in [SECURITY.md](SECURITY.md).

## 1. gRPC: the control plane

Six `.proto` files, one per package, compiled by
[`common/build.rs`](../common/build.rs) into `common::proto::<pkg>::v1`
(client and server stubs). The protobuf schema is the boundary between
modules: no internal Rust type crosses a crate. Three services have a
server; two (`QkcControl`, `QudittoControl`) exist only as schema — nothing
serves or calls them, the QKC and quditto expose HTTP and the wire instead.

### `SdnControl` — served by the SDN on 19000 ([`sdn.proto`](../proto/sdn.proto))

| RPC | State | Caller |
|-----|-------|--------|
| `GetSaeBinding` | implemented | the DKMS, to learn which DKMS and ORR serve a SAE (cached, `sae_binding.ttl_secs`) |
| `GetOrrPath` | implemented | the ORR, only for the multi-hop onion modes (`max_hops = -1` or `≥ 2`) and only when the header carries no `orr_path`; the answer is cached per destination |
| `StreamTopology` | implemented; at most 256 subscribers | every DKMS, and the ORR when its SDN channel is up, to invalidate their caches on a version bump |
| `ComputePath` | implemented; every policy resolves by hops | nobody at runtime (diagnostics) |
| `CheckAdmission` | stub, always allowed | nobody |
| `PutTopology`, `UpdateLink` | `UNIMPLEMENTED` on purpose | the graph is built from announcements; a whole-graph push would be a second source of truth that cannot expire nodes. Capacity changes go through `POST /link-capacity` |
| `ReportCapacity`, `ReportDkmsMetrics` | `UNIMPLEMENTED` | capacity travels in the QKC announcement, demand in `POST /demand` |

### `OrrControl` — served by the ORR on 20003 ([`orr.proto`](../proto/orr.proto))

| RPC | State | Caller | Authorisation |
|-----|-------|--------|---------------|
| `SendMessage` | implemented | its DKMS: one sealed transport key per call, with `max_hops` and `grade` | caller's certificate must be in `served_dkms` (when configured) |
| `StreamDeliveries` | implemented | its DKMS subscribes and receives every frame the QKC delivers to this node | same |
| `GetPublicKey` | implemented | a peer ORR at bootstrap | response signed with the node certificate (`signing_certs`), verified against the network CA and the SAN `dkms://<orr_id>`; with the legacy `sign_secret_seed` the signature is checked against `peer_verify_keys` instead |
| `EstablishSecret` | implemented | the lex-smaller ORR of a pair, to agree the `bootstrap_secret` | body `from` must equal the mTLS identity, else `PERMISSION_DENIED` |
| `RequestEphemeralKey`, `EstablishEphemeralSecret` | implemented | the lex-smaller ORR, once per `master_secret` epoch | HMAC over `bootstrap_secret` plus the same `from` binding |
| `OpenCircuit`, `CloseCircuit`, `GetCircuit`, `ListCircuits`, `Relay` | `UNIMPLEMENTED` | — | onion layers are built per message ([`orr/src/onion.rs`](../orr/src/onion.rs)); there are no circuits |

### `DkmsControl` — served by the DKMS on 20007, loopback ([`dkms.proto`](../proto/dkms.proto))

`RegisterSae` (sets a SAE's token-bucket limits), `DeregisterSae`, `ListSaes`
(returns an empty stream), `Drain` (wipes every buffer; semantics in
[dkms/README.md § DkmsControl](../dkms/README.md#dkmscontrol)),
`Health`, `GetBufferState`. No authentication: the renderer binds it to
`127.0.0.1` whatever `listen_ip` says, only `control_addr` in `node.yml` opens
it, and the binary warns at boot when the address is not loopback.

### TLS on the gRPC planes

| Channel | Default | Switch | Identity |
|---------|---------|--------|----------|
| DKMS → ORR, ORR ↔ ORR (20003) | mTLS | `grpc_tls` in the ORR, `southbound.orr_tls` in the DKMS, both `true`; the DKMS rewrites `orr_endpoint` from `http://` to `https://` at load, the ORR rewrites peer URLs the same way (`grpc_tls::peer_url`) | the ORR presents its node certificate and requires a network-CA client certificate; an ORR with `grpc_tls` and no `[tls]` refuses to start |
| DKMS → SDN (19000) | mTLS when the SDN has `[tls]` (`control_tls`, on by default in the renderer) | `sdn_endpoint` scheme | the DKMS presents its node certificate when the endpoint is `https://` |
| ORR → SDN (19000) | optional: `GetOrrPath` and `StreamTopology` only; the channel is opened once at boot and the ORR runs without it | `sdn_url` | [`orr/src/sdn_client.rs`](../orr/src/sdn_client.rs) sets no client TLS configuration, and tonic refuses an `https://` URL without one — so when the renderer writes `sdn_url` as `https://` (`control_tls`, the default) this channel does not come up |
| DkmsControl (20007) | plaintext | — | none; loopback |

Every TLS endpoint in the workspace runs on the provider of
[`common::tls_pqc`](../common/src/tls_pqc.rs) — the axum listeners through
`common::tls` (TLS 1.3 only), the tonic servers and channels and the reqwest
clients through the process default that `ensure_process_default` installs
at boot: the single key-exchange group `X25519MLKEM768`, ML-DSA-65
certificates (classical certificates only with
`DKMS_TLS_ACCEPT_CLASSICAL_CERTS=1`). Each binary
self-checks a handshake with its own identity at boot and aborts otherwise
([engineering notes](engineering-notes.md#active-known-issues--gotchas), "TLS
negotiates ONLY `X25519MLKEM768`"). Identity is the SAN URI
`dkms://<node_id>` of the certificate
([`common::cert_identity`](../common/src/cert_identity.rs)), never a field of
the body; where a body names a sender (`from`), the two must match.

### Dial defaults

`common::ipc::grpc::DialOpts::default()` is the intended baseline: connect
timeout 2 s, request timeout 10 s, HTTP/2 keepalive every 15 s with a 5 s
timeout, `TCP_NODELAY`, lazy connect, optional `ClientTlsConfig`. No module
uses it yet: each client builds its tonic `Endpoint` directly and connects
eagerly, with its own timeouts (the DKMS from `southbound.connect_timeout_ms`
1500 and `rpc_timeout_ms` 3000; the ORR's SDN channel 1500 / 3000 ms). The
DKMS dials the SDN and the ORR at boot (30 and 20 attempts one second apart)
and continues without whichever never answered; `StreamTopology`
subscribers reconnect with backoff (250 ms to 5 s).

## 2. HTTP

Five surfaces, all axum. Where a listener is mTLS, the client identity is
the SAN URI of its certificate and the handlers compare it with what the
body claims; "sdn identity" below is the certificate `gen-certs.sh sdn <ip>`
emits (SAN `dkms://sdn`).

### SDN admin — 19002 ([`sdn/src/http_api.rs`](../sdn/src/http_api.rs))

With `[tls]` (what `control_tls`, on by default, renders) the port is mTLS
with a mandatory network-CA client certificate, the mutating routes bind the
body to that certificate, and the read-only routes are also served in
plaintext on `http_ro_addr` if set (`http_ro_port` in `node.yml`, bound to
`127.0.0.1`). Without `[tls]` everything is plaintext and no identity is
checked.

| Group | Route | Caller | Requires |
|-------|-------|--------|----------|
| Registration | `POST /register/qkc` | every QKC, every `sdn_announce_secs` (30 s; from 2 s with backoff while an edge is pending or the SDN is unreachable) | certificate `qkc-<id>` |
| | `POST /register/orr` | every ORR | certificate `<orr_id>` |
| | `POST /register/dkms` | every DKMS, with the SAEs it serves | certificate `<dkms_id>` |
| Rate and demand | `GET /rate/{dkms_id}` | the DKMS, every `rate_refresh_ms`: per-peer ENC/DEC rates for its generator | read-only |
| | `POST /demand` | the DKMS, batched `(level, capacity, drain_rate)` per peer | certificate `<dkms_id>` of the body |
| | `GET /demand` | operator | read-only |
| SAE | `POST /sae`, `POST /sae-bulk` | operator, tests (the normal path is the `saes` list of the DKMS announcement) | certificate of the owning `dkms_id` |
| | `PUT /sae/{sae_id}`, `DELETE /sae/{sae_id}` | operator | certificate of the DKMS that currently serves the SAE |
| | `GET /sae/{sae_id}/binding`, `GET /sae-bindings/{dkms_id}` | operator, tests | read-only |
| Link capacity | `POST /link-capacity` | operator | sdn identity or one of the two endpoint QKCs |
| Paths | `POST /paths` | operator | read-only group |
| Read-only | `GET /healthz`, `/topology`, `/qkcs`, `/orrs`, `/dkms`, `/saes`, `/links`, `/wcmp` | operator, tests | read-only group |

Mutating bodies are capped at 1 MiB. The announcement response carries the
peer set the module must hold (sorted by id), the outcome per declared link
and, for an anchor already taken, `accepted: false` with a reason; the
announcer keeps retrying. Graph rules in
[auto-configuration.md](auto-configuration.md).

### QKC admin — 20002 ([`qkc/src/http_admin.rs`](../qkc/src/http_admin.rs))

| Route | Caller | Notes |
|-------|--------|-------|
| `POST /forwarding-table` | the SDN, whenever the topology version or the rate snapshot changes (checked every 200 ms, so in practice at every recompute, `mcf_period_ms` = 5 s) | `{"replace": {dest: [{qkc_id, weight}, ...]}}` (WCMP) or `{"updates": {...}, "removes": [...]}`; `replace_qkd` is the QKD-only table (sent only with `SDN_DUAL_GRADE_TABLES`); a bare integer still parses as one next hop of weight 1 |
| `GET /forwarding-table` | operator | current snapshot |
| `GET /stats` | operator, tests | intake, relay and per-link key-store counters |
| `GET /healthz` | operator | liveness |

With `[tls]` the listener is mTLS with a mandatory network-CA client
certificate and `POST /forwarding-table` is accepted **only** from the sdn
identity: whoever writes the table decides where every OTP frame goes. The
SDN pushes over `https` if and only if it has `[tls]` itself, so a
one-sided configuration fails loudly on both ends. Without `[tls]` the port
is plaintext and anyone can push.

### DKMS ETSI GS QKD 014 — SAE plane, 20005 ([`dkms/src/etsi_http/v014.rs`](../dkms/src/etsi_http/v014.rs))

mTLS, client CA `sae-ca`, identity `urn:dkms:sae:<id>`. The three endpoints
of the standard, `enc_keys` and `dec_keys` in the two forms the spec allows,
plus `GET /healthz`:

| Endpoint | Forms |
|----------|-------|
| `/api/v1/keys/{slave_SAE_ID}/status` | `GET` |
| `/api/v1/keys/{slave_SAE_ID}/enc_keys` | `POST` with the JSON body; `GET ?number=N&size=S` |
| `/api/v1/keys/{master_SAE_ID}/dec_keys` | `POST` with the JSON body; `GET ?key_ID=<uuid>` (one key, no extensions, as the spec limits it) |

### DKMS ETSI GS QKD 020 — peer plane, 20006 ([`dkms/src/etsi_http/v020.rs`](../dkms/src/etsi_http/v020.rs))

mTLS, client CA `net-ca`, identity `dkms://<node_id>`. A valid network
certificate that is not in this DKMS's peer registry gets `403` until the SDN
has handed the peer out in an announcement response.

| Route | Caller | What travels |
|-------|--------|--------------|
| `POST /kmapi/v1/ext_keys` | the master's DKMS | the session key, OTP-wrapped with a transport key; the per-key extension names `transport_key_id`; the response is the ETSI-020 ack container |
| `POST /kmapi/v1/ext_keys/ack` | the receiver of a transport key | the `key_ids` acknowledged, so the sender moves them from `ack_pending` to `buffer_enc`; the sender is the certificate, never a body field (`ack_transport = etsi020`, the default) |
| `POST /kmapi/v1/e2e/kem` | either DKMS of a pair | not ETSI: the ML-KEM agreement behind the e2e seal — an ephemeral public key in, ciphertext plus a responder-assigned epoch out ([`dkms/src/e2e.rs`](../dkms/src/e2e.rs)) |
| `GET /kmapi/v1/versions` | any peer | `["1.0"]` |
| `GET /healthz` | operator | liveness |

The plain TCP ACK socket on 20009 (newline-delimited JSON `{"from",
"key_ids"}`, no authentication) is the legacy transport of the same
acknowledgement: off out of the box (`ack_transport = etsi020`,
`ack_socket_listen = false`), only for a mixed migration, set at both ends.

### quditto ETSI GS QKD 014 — 20010 ([`quditto/src/server.rs`](../quditto/src/server.rs))

quditto simulates one KME for one link and serves the QKCs at both of its
ends through the same client ([`qkc/src/kme.rs`](../qkc/src/kme.rs)) the QKC
uses against a real KME: `GET /api/v1/keys/{sae_id}/status` (stock and
ceiling, what the rate estimator reads), `GET .../enc_keys?number=N&size=B`,
`GET .../dec_keys?key_ID=<uuid>`, `POST .../dec_keys`, `GET /healthz`.
quditto also answers `Accept: application/octet-stream` with a binary body
(crate [`etsi`](../etsi/README.md)) instead of JSON+base64, but the QKC
client always asks for JSON, which any KME understands. mTLS is the default
(`--tls on`, client CA `net-ca`): the OTP pads travel in these bodies.
`QUDITTO_TLS=off` is an explicit opt-out the binary logs as such, acceptable
only with the consuming QKC on the same host.

## 3. Binary TCP: the wire

One frame format, the [`wire`](../wire/README.md) crate, on two planes:

| Plane | Port | Connection model |
|-------|------|------------------|
| QKC ↔ QKC | 20000 (`peer_listen`) | one persistent TCP connection per peer and direction: each QKC writes over the connection it opened and reads on the one its neighbour opened; the queue in front of each writer drops on overflow rather than growing |
| ORR ↔ QKC | 20001 (`local_listen`, `127.0.0.1` unless `local_bind`) | one bidirectional connection the ORR opens to its own QKC and reopens with backoff |

`common::ipc::binary_tcp` re-exports the crate (kept for compatibility;
nothing in the workspace uses that path today); the byte layout is in
[wire/README.md](../wire/README.md) and the API in the crate doc
(`make doc-open`).

### Why binary

On a `qkd` link the payload is OTP-encrypted in blocks of `key_size_bits / 8`
(32 bytes with the `node.yml` default of 256 bits) and **every block spends
one QKD key**. A 32-byte
transport key must therefore stay exactly 32 bytes of payload: tags, epochs,
counters and ids ride in the cleartext headers, which cost no key material.
Framing is a 10-byte prefix and length-prefixed fields, nothing to parse per
byte. Getting this wrong was measured — an AEAD tag in the payload halved the
QKD throughput ([engineering
notes](engineering-notes.md#active-known-issues--gotchas), "On an OTP link,
every byte added to the payload costs QKD key material").

### The layers and the frame kinds

A frame is a 10-byte prefix (`MAGIC`, `kind`, `grade`, `total_len`), the
QKC's fixed fields, `epoch_id`, two cleartext msgpack headers (ORR, DKMS)
and the payload. Each layer owns its fields and touches nothing else: the
QKC rewrites its own per hop and propagates `epoch_id` and the two upper
headers byte for byte; the payload is the only field that spends key
material. Which layer writes and reads each field, the byte layout and the
full table of frame kinds (`0x01`–`0x2A`: data with and without the link-MAC
trailer, the ORR↔QKC local kinds, the KME NOTIFY, the ML-KEM handshake and
the responder's resync request) are in [wire/README.md](../wire/README.md).
A kind the receiver does not know is ignored, which is what lets `pqc_auth`
and `frame_auth` roll out link by link (`off`, `prefer`, `require`). A relay
that rebuilt frames without copying `epoch_id` turned every onion into
epoch 0 and broke the first live rotation; the propagation is pinned by a
test in `qkc/src/relay.rs`.

### The link MAC trailer

`frame_auth` puts `session(8) ‖ counter(8) ‖ tag(32)` at the end of the
payload of `0x04`, `0x05` and `0x25`. The tag is an HMAC-SHA256 over the
whole frame, keyed by `HKDF(root, session)`. The root is the epoch secret
of the link's ML-KEM handshake (every `pqc` link; a `qkd` link when the
QKC has `[tls]`), and `session` is then that epoch; an explicit `link_psk`
takes precedence as a fixed root, with `session` a random incarnation per
process run. `counter` feeds a sliding anti-replay window. The trailer is
appended **after** the OTP encryption and stripped **before** decryption, so
it never enters the block chunker and costs no key material; it is verified in the connection reader,
in arrival order, before anything is queued ([engineering
notes](engineering-notes.md#active-known-issues--gotchas), "The link MAC and
the anti-replay window are checked in the connection READER"). Primitive:
[`common::crypto::frame_mac`](../common/src/crypto/frame_mac.rs).

### Size cap

The reader rejects any `total_len` above **1 MiB** before allocating and
reads the body in 16 KiB chunks, so memory grows with what arrives, not with
what an unauthenticated prefix announces; the worst legal frame is far below
that. `write_frame` validates every field against its length prefix before
encoding, and `read_frame_body_timeout` bounds how long a body may take
once its prefix has arrived (an idle link waits for a prefix without limit).

## 4. Ports

What [`docker/render_config.py`](../docker/render_config.py) writes for each
role when `node.yml` has no `ports:` block (host networking; the override is
only for several nodes of one role on one machine). The firewall view is in
[docker/README.md](../docker/README.md#ports-who-connects-to-whom).

| Module | Port | Plane | Bind | Who connects |
|--------|------|-------|------|--------------|
| SDN | 19000 | gRPC `SdnControl` | `listen_ip` (`0.0.0.0`) | every DKMS; the ORR's optional channel does not come up under `control_tls` ([TLS on the gRPC planes](#tls-on-the-grpc-planes)) |
| SDN | 19002 | HTTP admin | `listen_ip` | every QKC, ORR and DKMS; the operator |
| SDN | `http_ro_port` (e.g. 19003) | HTTP read-only mirror, plaintext | `127.0.0.1` unless `http_ro_bind` | the operator; rendered only if set |
| SDN | 19010 | `/metrics` | `listen_ip` | Prometheus |
| QKC | 20000 | wire, peer | `listen_ip` | the neighbour QKCs, both directions |
| QKC | 20001 | wire, local | `127.0.0.1` unless `local_bind` | its ORR |
| QKC | 20002 | HTTP admin | `listen_ip` | the SDN (forwarding push); the operator for `GET` |
| ORR | 20003 | gRPC `OrrControl` | `listen_ip` | its DKMS and the peer ORRs |
| ORR | 20004 | `/metrics` | `listen_ip` | Prometheus |
| DKMS | 20005 | HTTPS ETSI-014 | `listen_ip` | the SAEs |
| DKMS | 20006 | HTTPS ETSI-020 | `listen_ip` | the peer DKMSs |
| DKMS | 20007 | gRPC `DkmsControl` | `127.0.0.1` unless `control_addr` | the operator, on the host |
| DKMS | 20008 | `/metrics` | `listen_ip` | Prometheus |
| DKMS | 20009 | legacy ACK socket | rendered, not listening (`ack_socket_listen = false`) | nobody by default |
| quditto | 20010 | HTTPS ETSI-014 (`QUDITTO_LISTEN`) | `listen_ip` | the QKCs at both ends of the simulated link |

The `/metrics` exporters (`common::metrics`) have no authentication and
answer on any path: internal network only. The QKC has no metrics port; its
counters are on `GET /stats`.

## 5. The rule

Two transports for anything new, and no third. A new module-to-module RPC
is gRPC with its schema in [`proto/`](../proto/), even when an HTTP route
would be quicker to write; the binary wire is only for frames that carry key
material between QKCs or between an ORR and its QKC. The HTTP surfaces
exist because their protocols are HTTP (ETSI 014 and 020, the KME) or
because an operator with `curl` is a client (the SDN and QKC admin, on which
the announcements and the forwarding push also ride); they are not a
licence for a third transport. Modules share the protobuf schema and the
`wire` crate, never their internal types
([engineering notes](engineering-notes.md#things-to-not-do)).

## Further reading

[architecture.md](architecture.md) (the flows and the glossary),
[SECURITY.md](SECURITY.md) (trust roots, table of planes),
[auto-configuration.md](auto-configuration.md) (what an announcement carries),
[engineering-notes.md](engineering-notes.md) (the measured failures behind
the invariants named here); the shared crates [wire](../wire/README.md) and
[common](../common/README.md); the module READMEs [qkc](../qkc/README.md),
[orr](../orr/README.md), [sdn](../sdn/README.md), [dkms](../dkms/README.md),
[quditto](../quditto/README.md); the schemas in [`proto/`](../proto/).
