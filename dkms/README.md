# DKMS — the key server of a node

The DKMS is the module a node shows to its users. It serves keys to SAEs over
ETSI GS QKD 014, agrees keys with the DKMSs of other nodes over ETSI GS QKD
020, and holds in RAM the per-peer buffers of transport keys that make those
agreements information-theoretically wrapped. It is the cryptographic endpoint
of the relationship between two nodes: the ORR and the QKCs carry its material
but cannot read it. It never sees a link, a KME or a forwarding table; it only
knows its ORR, the SDN and its peers.

## What it does

- **Serves ETSI-014 to SAEs** on `listen.sae_addr` (port 20005), HTTPS with
  mTLS (HTTP/2 or HTTP/1.1) against `tls.sae_client_ca`. Every request is
  authorised against `sae_bindings` (fail-closed: an authenticated SAE this
  node does not serve gets 404 `UnknownSae`). Handlers in
  `src/etsi_http/v014.rs`.
- **Exchanges session keys with peer DKMSs over ETSI-020** on
  `listen.peer_addr` (port 20006), HTTPS with mTLS (HTTP/2 or HTTP/1.1)
  against `tls.peer_dkms_ca`. A valid network certificate is not enough: the
  peer must be in the peer registry (403 otherwise). Handlers in
  `src/etsi_http/v020.rs`.
- **Keeps the transport buffers**: `buffer_enc[peer]` (what this node spends
  when it sends to `peer`) and `buffer_dec[peer]` (the twin copies of what
  `peer` spends towards this node). RAM only, zeroised on drop, size
  `buffer.capacity_per_peer` (4096). `src/state/pool.rs`.
- **Fills them**: the generator produces transport keys at the rate the SDN
  hands it (`GET /rate/<dkms_id>`) and pushes them to each peer through the
  ORR. `src/control/generator.rs`.
- **Seals every transport key end to end** with AES-256-GCM under a per-pair
  ML-KEM secret (`src/e2e.rs`); the ORR and the QKCs are relays.
- **Talks to the SDN**: announces itself (`POST /register/dkms`, also its
  heartbeat), reports measured SAE demand (`POST /demand`) and resolves where
  each SAE lives (`GetSaeBinding`). `src/southbound/`.
- **Exposes a management gRPC** (`DkmsControl`, port 20007) on loopback only;
  its `Drain` RPC wipes every buffer. `src/grpc_server.rs`.
- **Reports its state** once every 5 s per peer in the `generator.state` log
  line (see [Health and diagnostics](#health-and-diagnostics)).

## How it works

### Two planes, two CAs

The DKMS runs two HTTPS listeners with the same server certificate but
different client verifiers (`src/etsi_http/mod.rs`): the SAE plane verifies
against `sae-ca`, the peer plane against `net-ca`. Keeping them on separate
ports means a SAE certificate can never pass as a DKMS and vice versa. The
client identity is the SAN URI of the certificate (`urn:dkms:sae:<id>` for
SAEs, `dkms://<node_id>` for DKMSs; a DNS SAN or the CN with the bare id is
the fallback, `src/etsi_http/auth.rs`), never a field of the request body.
TLS 1.3 negotiates only the hybrid `X25519MLKEM768` exchange and ML-DSA-65
certificates; every binary self-checks this at boot.
The trust model is in [SECURITY.md](../docs/SECURITY.md#11-tabla-de-planos).

### Transport keys: generator → ORR → peer → ACK

The generator runs one token bucket per peer (`tick_ms` = 100 ms, at most
`max_tokens_per_peer_per_tick` = 32 tokens per tick, so 320 keys/s per peer
whatever the SDN says). Every `rate_refresh_ms` (1 s) it polls
`GET /rate/<dkms_id>` on the SDN's HTTP admin and uses
`max(sdn_rate, default_fill_rate_keys_per_s)` as the effective rate, capped
to `max_fill_rate_keys_per_s` when that is above 0 (both default to 0). Each
tick it decides a batch per peer, capped to the buffer headroom
`capacity_per_peer − (enc + ack_pending)`, then emits to all peers
concurrently, bounded by `max_emits_in_flight` (128), the only
back-pressure on the ORR path (why concurrent and why bounded: [engineering
notes](../docs/engineering-notes.md#active-known-issues--gotchas), "The
generator emits to every peer at once").

A key is `key_size_bytes` (32) random bytes with a UUIDv4 `key_id`. It goes
into `ack_pending[peer]` with a deadline of `ack_timeout_ms` (30 s), is sealed
(next section) and handed to the ORR as a `DKMS_BUFFER` message whose
`header_dkms` carries `key_id`, `key_size_bits`, `sae_origin` (this node),
`incarnation`, the seal fields and the message type. The grade of the key is
QKD unless the last `/rate` answer reported no QKD path to that peer (then
PQC; before the first answer, QKD); the ENC side is split by grade so that a
`strict_qkd` request never consumes a PQC-grade key.

The receiver (`handle_orr_delivery_buffer` in `src/service.rs`) drops any
delivery whose source is not a known peer, checks the payload length, opens
the seal, stores the key in `buffer_dec[source]` and acknowledges. ACKs travel
by default over ETSI-020: batches of up to 32 `key_id`s or 50 ms are POSTed to
the peer's `/kmapi/v1/ext_keys/ack`, where the sender identity is the client
certificate (`ack_transport = etsi020`). The legacy plain-TCP socket on port
20009 is off (`ack_socket_listen = false`) and only exists for mixed
migrations. On the sender, an ACK moves the entry from `ack_pending` to
`buffer_enc[peer]`; an entry that reaches its deadline is zeroised and counted
as `expired`.

### The end-to-end seal

Each `DKMS_BUFFER` is AES-256-GCM sealed by the sending DKMS under
`master[epoch]`, a secret agreed per pair with ML-KEM (`transport_e2e.suite`,
`ml-kem-768`) over the peer plane: `POST /kmapi/v1/e2e/kem` carries an
ephemeral public key, the responder encapsulates and **assigns a random epoch
number**, so two concurrent agreements yield two epochs and never one epoch
with two secrets. Key and nonce are derived from `master[epoch]` and `key_id`;
tag, epoch and counter ride in `header_dkms` (`e2e_tag`, `e2e_epoch`,
`e2e_ctr`) so the payload stays 32 bytes and costs one QKD key per hop on an
OTP link. The AAD covers the whole header plus origin and destination, which
is why the receiver opens **before** acting on `incarnation` or any other
header field. Whoever lacks an epoch asks: the sender when it first emits, the
receiver when it sees an epoch it does not hold (`recv_no_epoch`, one attempt
per 2 s per peer). Rotation by time (`rekey_secs`, 3600) is driven by the
lexicographically smaller peer; rotation by volume (`rekey_keys`, 100 000) by
either side. The last `epoch_history_keep` (4) epochs stay open for keys in
flight. Design and rationale: `src/e2e.rs` and the [engineering
notes](../docs/engineering-notes.md#active-known-issues--gotchas).

### Session keys: enc_keys → ext_keys → dec_keys

The sequence — master `enc_keys`, one transport key popped per session key
and destination, OTP wrap, ETSI-020 `ext_keys` to the peer DKMS, `dec_keys`
by each slave — is walked step by step in
[architecture.md § Flow 2](../docs/architecture.md#4-flow-2-a-sae-asks-for-a-key).
What this module adds to it:

- The master is authorised against `sae_bindings`; each target SAE is
  resolved to its DKMS through the binding cache; `strict_qkd` towards a
  peer the SDN reports as QKD-unreachable is refused; admission is a
  per-(peer, SAE) token bucket whose refill is the SDN rate shared among the
  SAEs active on that peer (429 when exhausted).
- Status codes: 503 `transport buffer empty` when `buffer_enc[peer]` has no
  key; 502 when a send to a peer fails (only the failed destinations are
  refunded, the material sent is spent); a failure *before* any send puts
  the popped transport keys back at the front of the buffer and refunds the
  tokens. A peer answering `transport key … not in buffer_dec` means this
  side's `buffer_enc[peer]` is stale: it is cleared so the generator
  rebuilds it.
- Config: `ttl_seconds` = `pending.default_ttl_secs` (86400) travels as a
  mandatory extension; sends to all peers run in parallel with
  `request.peer_send_timeout_ms` (1500); a digest mismatch on the receiver
  counts as `recv_corrupt` and is not acknowledged; the pending store is
  swept every `pending.sweep_interval_secs` (30) and an entry disappears
  when every authorised SAE has collected it or its TTL expires.

### SAE bindings, peers and demand

`sae_bindings` (`sae_id: dkms_id`) is required in practice (the renderer and
the binary only warn without it, but every SAE request then gets 404): it is
the list this node authorises SAEs against and the list it announces to the
SDN. Where a *remote* SAE lives is resolved through `SaeBindingCache`
(`src/sae_binding.rs`):
`GetSaeBinding` on the SDN's gRPC, cached with `sae_binding.ttl_secs` (60),
single-flight so concurrent requests share one lookup, a 5 s negative cache,
and invalidated wholesale when the SDN's `StreamTopology` reports a change.
Without an SDN the static `[sae_bindings]` table is the resolver. On the SDN,
the announcement (bound to the announcing certificate) claims only SAEs that
are free or already its own; a SAE owned by another DKMS is not moved until
its owner releases it.

Peers live in `PeerRegistry` (`src/peers.rs`). The `peers:` of `node.yml` are a
seed and a floor: the announce response carries the peer set the SDN derives
from the graph, adds are applied, and the SDN may remove only what it added.
Local policy per peer (`max_hops`, `orr_path`, `security_level`, `sni`) is
never overwritten; the SDN does refresh a seeded peer's `endpoint` and
`orr_id`. Why a floor: see [peers ride back on the
announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement).

Every `generator.demand_refresh_ms` (1 s) the DKMS reports to the SDN, per
peer, its buffer level, capacity and the EWMA of SAE demand measured **before**
bucket admission (`src/demand_tracker.rs`), so a rate-limited SAE still shows
as demand instead of starving itself.

### When a peer restarts

Buffers are RAM only, so a restart leaves both ends holding halves of pairs
the other no longer has. Every `DKMS_BUFFER` carries the sender's
`incarnation`, a random id per process run. When a peer's incarnation changes
(the first sighting is not a restart), the receiver drops its `buffer_enc`,
`buffer_dec` and `ack_pending` for that peer, logs a warning, and the generator
refills. The detector is the restarted node's own refill traffic: it boots
with an empty ENC and emits at once. Wipes are rate-limited to one per 30 s per
peer. The older reactive path (the `TransportKeyMissing` rejection on the next
SAE request) still covers a peer that comes back with its generator disabled.
Measurement and rationale: [engineering
notes](../docs/engineering-notes.md#active-known-issues--gotchas).

### DkmsControl

`proto/dkms.proto` defines `RegisterSae` (sets a SAE's limits on the legacy
per-master bucket, which only admits when no generator is running),
`DeregisterSae`, `ListSaes`, `Health`, `GetBufferState` and `Drain`. `Drain`
closes admission (new requests get 503), waits for in-flight requests up to
`grace_seconds` (default 5 s), then clears the pending store and every
transport buffer; admission is never reopened, so the DKMS answers 503 until
it is restarted. The RPC list and the loopback rule (no authentication, so
the renderer binds it to `127.0.0.1`):
[ipc.md § DkmsControl](../docs/ipc.md#dkmscontrol--served-by-the-dkms-on-20007-loopback-dkmsproto).

## Deployment

Minimal `node.yml` (from [`docker/examples/node.dkms.yml`](../docker/examples/node.dkms.yml)):

```yaml
node_id: "dkms-1"              # must equal the cert file name: certs/dkms-1.crt + .key
advertise_ip: "10.0.0.11"      # routable IP of this machine; in the cert SAN; announced to the SDN
orr_addr: "127.0.0.1:20003"    # this node's ORR (gRPC, mTLS by default)
orr_id: "orr_1"                # optional: the renderer derives orr_<n> from node_id dkms-<n>; the SDN places this DKMS under it
sdn_endpoint: "https://10.0.0.100:19000"
sae_bindings:                  # required: the SAEs this node serves
  sae_1: dkms-1
peers:                         # optional seed; the announce response adds the rest
  dkms-2:
    endpoint: "10.0.0.12"      # ip or ip:port; peer port 20006 by default
    orr_id: "orr_2"
```

Optional fields and their defaults (the renderer's, `docker/render_config.py`,
or the binary's where the renderer emits nothing): `orr_addr`
(`127.0.0.1:20003`), `orr_id` (`orr_<n>` for a `dkms-<n>`), `orr_tls` (true),
`sdn_announce_secs` (30), `security_level` (`qkd_prefer`;
also `strict_qkd`, `no_worry`), `fill_rate` (0 = only the SDN rate),
`capacity_per_peer` (4096), `transport_e2e` (`suite` ml-kem-768, `rekey_secs`
3600, `rekey_keys` 100000, `epoch_history_keep` 4, `replay_window` 1024),
`ack_transport` (`etsi020`), `ack_socket_listen` (false), `control_addr`
(unset = 127.0.0.1), `sae_authorization` (true), `certs_dir` (`/config/certs`),
`listen_ip` (0.0.0.0), `ports`, `control_tls` (true; sets the scheme of an
`sdn_endpoint` written without one) and `extra` (free-form TOML).

| port | listener | who connects | transport |
|------|----------|--------------|-----------|
| 20005 | SAE plane (ETSI-014) | SAEs of this node | HTTPS mTLS, client CA `sae-ca` |
| 20006 | peer plane (ETSI-020, e2e KEM, ACKs) | the other DKMSs | HTTPS mTLS, client CA `net-ca` |
| 20007 | `DkmsControl` gRPC | operator, same host | plaintext, `127.0.0.1` |
| 20008 | `/metrics` | Prometheus | plaintext HTTP, no auth |
| 20009 | legacy ACK socket | other DKMSs, only if `ack_socket_listen: true` | plain TCP, no auth |

Outbound it dials its ORR (20003, mTLS with the node certificate), the SDN
gRPC (19000), the SDN HTTP admin (19002, derived from `sdn_endpoint`) and
every peer's 20006.

Certificates, generated with `docker/gen-certs.sh dkms-1 <advertise_ip> ./certs`
(node certificate and `net-ca`) plus `docker/gen-certs.sh --sae <sae_id> ./certs`
(`sae-ca` and the SAE client certificate), mounted at `certs_dir`:

| file | role |
|------|------|
| `dkms-1.crt` / `dkms-1.key` | node certificate, signed by `net-ca`, SAN `URI:dkms://dkms-1`, `IP:<advertise_ip>`, `IP:127.0.0.1`, `DNS:localhost`. Presented on both planes and to ORR and SDN |
| `net-ca.crt` | verifies peer DKMSs (`tls.peer_dkms_ca`) and the SDN and ORR (`tls.control_plane_ca`). Common to the whole federation |
| `sae-ca.crt` | verifies SAE client certificates (`tls.sae_client_ca`, SAN `urn:dkms:sae:<id>`). May be per institution |

The announcement is `POST <sdn_http_url>/register/dkms` every
`sdn_announce_secs`, with `id`, `host` (`advertise_ip` + SAE port), `peer_addr`
(`advertise_ip:20006`), `orr_id` and `saes`, the sorted list of SAEs bound to
this node. It needs `advertise_ip` and `orr_id`; without them the DKMS serves
keys but does not appear in the topology. The response says `accepted`,
`waiting_for` (the ORR the SDN has not seen yet; the loop retries with backoff
from 2 s, doubling up to `sdn_announce_secs`), `reason` when the ORR's QKC
already has a DKMS, and `dkms_peers` (`dkms_id`, `orr_id`, `endpoint`), which
is applied to the peer registry. The SDN gRPC is dialled with 30 retries of
1 s at boot; if it never answers, the static `sae_bindings` table is the only
resolver (any other SAE gets 404) until the DKMS is restarted. The ORR is
dialled with 20 retries of 1 s; without it the generator and the delivery
pump do not start, and nothing retries later: restart the DKMS.

Without containers: `scripts/run-dkms.sh` runs `cargo run --release -p dkms`
with `CONFIG_DIR=dkms/config`, loading `config/default.toml` ← `local.toml`
(gitignored) ← environment `DKMS__<section>__<key>` (an override of one nested
key merges into its section). `dkms/config/default.toml` is the development
config; every field is in `src/config.rs`.

Full procedure: [docker/README.md § 4. DKMS](../docker/README.md#4-dkms), the
[SAE certificate section](../docker/README.md#saes-certificates-and-etsi-014-smoke-test)
and the condensed [quick start](../docker/examples/quick_start.md#4-dkms).

## Health and diagnostics

At boot, `dkms https listening … plane="sae"` and `plane="peer-dkms"`, and,
once the ORR and the SDN answer, `orr deliveries pump connected` and
`anunciado a la SDN accepted=true`. With the default `ack_socket_listen =
false` the generator also warns `sin ack_endpoint anunciado`: it refers to
the legacy socket and is moot with ACKs over ETSI-020. Every 30 s each plane
that saw a handshake logs `tls.stats` (handshakes, failed, avg_ms, max_ms).
Every 5 s the generator logs one `generator.state` line per peer:

| field | meaning |
|-------|---------|
| `enc`, `dec` | keys in `buffer_enc[peer]` / `buffer_dec[peer]` |
| `ack_pending` | emitted to this peer, ACK not yet received |
| `emit_capacity` | `capacity_per_peer`; emission stops while `enc + ack_pending` reaches it |
| `emit_total`, `observed_keys_per_s` | cumulative keys that reached `buffer_enc` through ACKs, and their rate between two lines |
| `sdn_rate_keys_per_s` | the rate the SDN hands this pair |
| `sae_drain_keys_per_s` | the demand EWMA this DKMS reports for the pair |
| `rate_ceiling_keys_per_s` | the token-bucket ceiling (320 with defaults) |
| `emitted`, `emit_failed` | keys handed to the ORR / rejected by it or not sealable (no e2e epoch yet) |
| `acked`, `expired` | ACKs matched / entries that timed out without ACK |
| `ack_miss_peer`, `ack_miss_key` | ACK from an unknown peer id / for an already-expired (or duplicate) key |
| `enc_full` | ACKs discarded because `buffer_enc` was full |
| `recv`, `ack_sent`, `ack_send_failed` | the other direction: keys received from this peer and the ACKs sent back |
| `ack_no_endpoint` | receipts with no way to ACK; also bumped once per emission while no legacy socket endpoint is advertised, i.e. always with the defaults, so it tracks `emitted + emit_failed` and is not an error by itself |
| `recv_corrupt` | seal failed to open: altered in transit or divergent secret (also a `session_key_digest` mismatch on an incoming `ext_keys`) |
| `recv_no_epoch` | sealed with an epoch this side lacks (transient after a restart) |
| `recv_replayed` | valid tag, counter already seen |
| `dec_dropped` | `buffer_dec` refused the key (hard ceiling, 64 × capacity) |
| `ack_recv` | ACK batches received over ETSI-020 |
| `e2e_epoch` | the epoch this side seals with; `none` sustained means no agreement |
| `peer_ack_endpoint` | the legacy socket endpoint the peer announced, if any |

Healthy: `enc` and `dec` climb to `emit_capacity` and stay there,
`ack_pending` returns to 0, `expired`, `recv_corrupt`, `recv_replayed` and
`dec_dropped` stay at 0, `e2e_epoch` is a number. When `enc` is 0 and there is
a cause the counters can name, a `generator.diag` line spells it out.

Symptoms an operator will meet:

- **`ack_pending` and `expired` grow, `acked` stays at 0.** Keys leave but
  nothing comes back: either the peer never receives them (its `recv` is 0:
  the ORR/QKC path is down) or its ACKs cannot reach this node's port 20006.
  `ack_miss_peer > 0` is an identity mismatch between the peer's `node_id` and
  the `peers.<id>` key. The reading table is in
  [Diagnosing the DKMS-to-DKMS key cycle](../docker/README.md#diagnosing-the-dkms-to-dkms-key-cycle).
- **`e2e_epoch=none` with `recv_no_epoch` climbing.** The ML-KEM agreement is
  not converging: the peer's 20006 is unreachable from here or the certificate
  chains do not meet at `net-ca`.
- **A peer restarted.** Expected trace: a warning `el peer se ha reiniciado`
  naming the discarded `enc`, `dec` and `ack_pending`, then the buffers refill
  within about 30 s. Before this detector the pair sat at `dec=0 recv=0`
  forever ([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas)).
  A peer answering 503 `transport key … not in buffer_dec` to an `ext_keys`
  (the SAE's `enc_keys` gets a 502) is the reactive form of the same recovery.
- **SAEs get 404 `UnknownSae`** — `sae_bindings` does not list them for this
  `node_id`; when no binding points to this node at all, the renderer and the
  binary both warn at start. **429**
  means the per-SAE bucket is empty; check `sdn_rate_keys_per_s` (0 means the
  SDN never assigned a rate to that pair).

The Prometheus endpoint on 20008 registers no DKMS gauges; the log line above
is the state source.

## Where things live

| file | responsibility |
|------|----------------|
| `src/main.rs` | wiring: config, TLS, SDN/ORR clients with retries, generator, listeners |
| `src/service.rs` | `DkmsService`: ETSI-014/020 logic, envelope build, ORR delivery handling, incarnation |
| `src/etsi_http/` | the two mTLS planes (`mtls.rs`), identity extraction (`auth.rs`), handlers (`v014.rs`, `v020.rs`), admission layer |
| `src/control/generator.rs` | token buckets, `/rate` polling, emission, ACK handling, `generator.state` |
| `src/control/ack_pending.rs`, `ack_socket.rs`, `flow_stats.rs`, `sae_buffer_bucket.rs` | keys awaiting ACK, ACK transports (ETSI-020 and legacy socket), per-peer counters, per-(peer, SAE) admission buckets |
| `src/peer_client.rs` | HTTP/2 mTLS client to peer DKMSs: `ext_keys`, `ext_keys/ack`, `e2e/kem` |
| `src/e2e.rs` | per-pair ML-KEM epochs, seal/open, rekey loop |
| `src/state/` | `pool.rs` (buffers per peer and grade), `buffer.rs` (zeroised FIFO), `pending.rs` (session keys awaiting `dec_keys`) |
| `src/peers.rs` | peer registry: node.yml seed plus what the SDN sends |
| `src/sae_binding.rs` | SAE → DKMS cache (TTL, single-flight, negative cache) |
| `src/southbound/` | `sdn_announce.rs`, `sdn_http.rs` (`/rate`, `/demand`), `sdn.rs` (gRPC), `orr.rs` (header keys, `incarnation`) |
| `src/demand_tracker.rs` | EWMA of SAE demand per peer |
| `src/grpc_server.rs` | `DkmsControl` (`Drain` and friends) |
| `src/config.rs`, `config/default.toml` | every field with its default; the development config (not every field) |

The rustdoc has the rest: `make doc-open`.

## Further reading

- [Architecture](../docs/architecture.md), [auto-configuration](../docs/auto-configuration.md), [IPC](../docs/ipc.md), [deployment](../docs/deployment.md)
- [docker/README.md § 4. DKMS](../docker/README.md#4-dkms), [ports](../docker/README.md#ports-who-connects-to-whom), [security and firewall](../docker/README.md#security-and-firewall), [SAE client requirements](../docker/README.md#sae-client-requirements-read-before-blaming-the-dkms)
- Engineering notes: [defaults](../docs/engineering-notes.md#defaults), [known issues and gotchas](../docs/engineering-notes.md#active-known-issues--gotchas) (generator bound, restart recovery, e2e seal, `DkmsControl`, demand eviction), [solver and fairness](../docs/engineering-notes.md#solver--fairness), [roadmap and measured state](../docs/engineering-notes.md#roadmap-and-measured-state)
- [SECURITY.md](../docs/SECURITY.md): [plane table](../docs/SECURITY.md#11-tabla-de-planos), [PKI](../docs/SECURITY.md#2-pki-objetivo), [phase 4 (ACKs over ETSI-020)](../docs/SECURITY.md#fase-4--plano-peer-dkms-eliminar-el-ack-socket--binding-en-ext_keys)
- [Campaign 2026-09](../docs/results/campaign-2026-09.md): 60 cells with 0 `recv_corrupt` and 0 byte-different ETSI-014 exchanges up to N=100
- The other modules: [ORR](../orr/README.md), [QKC](../qkc/README.md), [SDN](../sdn/README.md), [ETSI types](../etsi/README.md); tests: [local mesh](../tests/local-mesh/README.md), [testbed](../tests/testbed/README.md)
- Schema: [`proto/dkms.proto`](../proto/dkms.proto)
