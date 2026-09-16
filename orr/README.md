# ORR — the relay between a node's DKMS and its QKC

The ORR (Onion Routing Router) sits between the DKMS and the QKC of one node.
Its DKMS hands it transport keys over gRPC (`OrrControl`, mTLS by default);
the ORR wraps each one in a wire frame and pushes it into the co-located QKC
over the binary TCP local port, and whatever the QKC delivers for this node
comes back the same way and is streamed to the DKMS. It does not route: the
QKC's forwarding table decides the path, the ORR only names the destination
QKC. It never sees a transport key in the clear — the DKMS seals every
`DKMS_BUFFER` end to end before calling it (the [e2e seal](../dkms/src/e2e.rs)) —
and it never touches link key material, which belongs to the QKC. By default
(`max_hops = 0`) it is a relay; the onion modes add optional per-hop layers
on top of the already-sealed payload.

## What it does

- **Carries the DKMS's payloads to a destination ORR.** `SendMessage` names a
  destination `orr_id`; the ORR resolves it to a `qkc_id` through its peer
  directory ([`src/peers.rs`](src/peers.rs)) and emits a `FRAME_LOCAL_SEND`
  with `dest_final = qkc(dest)` ([`src/service.rs`](src/service.rs)). The
  DKMS's `app_header` travels untouched in `header_dkms_mp`; the ORR's own
  header goes in `header_orr_mp` ([`src/header.rs`](src/header.rs)).
- **Delivers what arrives for this node.** Each `FRAME_LOCAL_DELIVER` from
  the QKC becomes a `DeliveredMessage` (`origin`, `destination`, `payload`,
  `app_header`) on every open `StreamDeliveries` stream. The DKMS keeps one
  open (`orr.stream_deliveries subscribed subscriber=dkms-<node_id>`, i.e.
  `dkms-dkms-1` for `node_id: dkms-1`).
- **Preserves the key grade.** `SendMessageRequest.grade` (0 = QKD-grade,
  1 = PQC-grade) is written into the frame's reserved byte so the QKC picks
  the QKD-only or the full forwarding table ([`wire`](../wire/src/lib.rs),
  `GRADE_QKD` / `GRADE_PQC`).
- **Authorises its two gRPC surfaces separately.** `SendMessage` and
  `StreamDeliveries` accept only the certificate identities in
  `served_dkms`; the ORR-to-ORR RPCs require the body's `from` to equal the
  caller's mTLS identity, and `EstablishSecret` only takes a `from` that is
  in the peer directory ([`src/grpc_server.rs`](src/grpc_server.rs)).
- **Keeps a per-pair `master_secret` with every other ORR**, bootstrapped over
  ML-KEM and rotated by epochs ([`src/bootstrap.rs`](src/bootstrap.rs),
  [`src/rotation.rs`](src/rotation.rs)). Only the onion modes consume it.
- **Announces itself to the SDN** every `sdn_announce_secs` (default 30 s)
  with `POST /register/orr`, anchored to its `qkc_id`; the response carries
  the ORR peer set ([`src/sdn_announce.rs`](src/sdn_announce.rs)).
- **Emits a status line every 5 s**: `orr.state` with the aggregate counters
  and one `orr.peer` per peer ([`src/stats.rs`](src/stats.rs)).

## How it works

The end-to-end flows (SAE request, transport-key fill, ACKs) are described in
[architecture.md](../docs/architecture.md); this section covers the ORR's part.

### Send and receive (mode 0, the default)

1. The DKMS calls `SendMessage(destination = orr_B, payload, app_header,
   max_hops = 0, grade)`. The DKMS always sets `has_max_hops`, so
   `default_max_hops` in the ORR config only applies to other clients.
2. The ORR checks the caller against `served_dkms`, looks up `qkc_id(orr_B)`
   and builds a `FRAME_LOCAL_SEND` (`epoch_id = 0`, no `key_id` in the ORR
   header: the QKC delivers the payload exactly as received).
3. The frame goes down the persistent TCP connection to the QKC's local port
   ([`src/qkc_link.rs`](src/qkc_link.rs)). The QKC encrypts it per link and
   forwards it along its forwarding table; the ORR is not involved again.
4. At the destination, the QKC hands its ORR a `FRAME_LOCAL_DELIVER`. The ORR
   decodes its header, sees no `key_id`, and broadcasts a `DeliveredMessage`
   to its subscribers. The receiving DKMS opens the e2e seal.

If no DKMS is subscribed when a message arrives, it is dropped
(`orr.deliver no_subscribers`, debug level) and still counted in
`delivered`; `delivery_no_subscriber` only counts self-addressed
`SendMessage`s (destination = this ORR) that found nobody. A subscriber
slower than the stream loses messages rather than slowing the ORR down
(`orr.stream_deliveries lagged`, queue depth `deliver_queue_capacity`,
default 4096).

### The four `max_hops` modes

| `max_hops` | What the ORR does | Needs |
|---|---|---|
| `0` (default) | Relay: no ORR layer. | The destination's `qkc_id`. |
| `1` | One AES-256-GCM layer for the destination ORR. | A `master_secret` with the destination. |
| `-1` | One layer per ORR on the path the SDN returns (`GetOrrPath` over `sdn_url`), or the `orr_path` CSV in `app_header`. | A `master_secret` with every hop; the SDN's gRPC or the hint. |
| `>= 2` | As `-1`, but N hops chosen at random from that path, order preserved, destination always last. | Same. |

What a layer adds is path privacy, not confidentiality of the key: the
payload is already sealed by the DKMS. Each layer is AES-256-GCM under
`HKDF(master_secret, key_id)`; the outer layer's tag rides in the ORR header
(an inner layer's inside the layer that wraps it), never appended to the
payload, because on an OTP link every payload byte costs QKD material
([ipc.md § Why binary](../docs/ipc.md#why-binary)).
The AAD binds the layer to `key_id`, `epoch_id`, `max_hops`, the origin's
`session ‖ counter` and the DKMS header, and a per-origin replay window
([`src/onion_replay.rs`](src/onion_replay.rs)) rejects a captured layer.
Details in [`src/onion.rs`](src/onion.rs). The `OpenCircuit` / `Relay` RPCs
in the proto are stubs that answer `UNIMPLEMENTED`; layers are built per
message, without circuits.

### The per-pair `master_secret`

Every ORR generates an ML-KEM keypair at boot (`default_pqc_suite`,
`ml-kem-768`). It is ephemeral: a restart produces a new one, which is why the
trust anchor is the node certificate, not the key.

1. **Bootstrap.** For each peer, the ORR fetches the peer's public key with
   `GetPublicKey` (retrying with backoff, 250 ms to 30 s). The response is
   signed with the ML-DSA-65 key of the peer's node certificate and carries
   the chain; the caller verifies the chain against `control_plane_ca` and
   the SAN `dkms://<orr_id>`. With
   `bootstrap_trust = strict` (default) an unsigned or unverifiable
   announcement is rejected; `tofu` accepts it (and logs that it did) and is
   only meant for a deliberately plaintext deployment. Then only the
   lexicographically smaller `orr_id` encapsulates and calls
   `EstablishSecret`; the other side decapsulates. Both store the result as
   `bootstrap_secret`: the HMAC key of the rotation RPCs, and also the
   `master_secret` of epoch 0 until the first rotation replaces it. Logs:
   `orr.peer_pubkey bootstrap ok`, then `orr.bootstrap bootstrap_secret ok`
   on the initiator and `orr.establish_secret bootstrap_secret stored` on
   the responder.
2. **Rotation.** The initiator runs `RequestEphemeralKey` +
   `EstablishEphemeralSecret` every `rotation_period_ms` (default 3 600 000 =
   1 h), and once immediately after bootstrap so the pair leaves epoch 0 at
   once. Each rotation installs epoch N+1 from a fresh ephemeral ML-KEM
   keypair; the responder zeroises its ephemeral secret key after
   decapsulation, so past epochs cannot be recovered from a captured
   long-term key. `epoch_history_keep` (default 3, floor 2) epochs stay
   live per peer. Log: `orr.rotation committed`.
3. **Which epoch a frame uses** travels in the wire frame itself
   (`Frame.epoch_id`, u32 big-endian). The receiver looks up
   `master_secret[from][epoch_id]`; the QKC must propagate the field
   byte-for-byte (it once rebuilt frames with `epoch_id = 0` and every
   onion failed after the first rotation — see the
   [gotchas](../docs/engineering-notes.md#active-known-issues--gotchas)).

Invariants an operator should know, each the fix of a measured failure
([gotchas](../docs/engineering-notes.md#active-known-issues--gotchas)):

- **One establishment in flight per pair.** Two concurrent encapsulations
  leave the two ends with different secrets, with no error at establishment
  time — every layer between them then fails to open (`peel_failed`).
  Bootstrap, rotation and the passive re-bootstrap all take the same
  per-attempt guard.
- **One rotation task per pair**, on the initiator only.
- **A bootstrap resets the whole epoch history on both sides.**
- **A peer that answers `no bootstrap_secret` (or fails the rotation MAC:
  diverged secrets) has restarted.** The rotation loop re-bootstraps it
  itself (`orr.bootstrap rehecho`, backoff up to 30 s), so a restarted ORR
  converges without traffic and without restarting its peers — at the
  initiator's next rotation tick, i.e. up to `rotation_period_ms` later. In
  the onion modes a frame from a known peer with an epoch this side lacks
  also triggers a re-bootstrap, from whichever end receives it (after a
  failed attempt, retries are limited to one per 5 s per peer).

### Peers and the announce loop

The ORR announces `{id, host: {ip: advertise_ip, port: grpc}, qkc_id}` to the
SDN's HTTP admin. The SDN accepts it only once that QKC is registered
(`accepted: false, waiting_for: <qkc>` until then — boot order, not an
error) and rejects a second ORR on an occupied QKC (`accepted: false` with a
`reason`; the announcer keeps retrying). One ORR per QKC is a hard rule: two
of them would leave one unresolvable and its material encrypted for the wrong
receiver ([one ORR and one DKMS per QKC](../docs/engineering-notes.md#one-orr-and-one-dkms-per-qkc)).

An accepted announcement returns `orr_peers`: every other ORR in the network
(not just neighbours — a `master_secret` is end to end), sorted by id, with
`qkc_id` and `grpc_url`. The ORR registers each new peer and starts its
bootstrap; a peer the SDN no longer lists is forgotten with its material.
Peers from `node.yml` (`peer_grpc_addrs`) are a floor: they are never
removed and keep the URL the operator wrote, whatever the SDN says ([peers
ride back on the
announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement)).
The SDN hands out `http://` URLs; each ORR rewrites the scheme from its own
`grpc_tls`, which is why that setting must be the same on every ORR.

### Restarts and broken links

- **The QKC connection drops**: the supervisor reconnects with backoff
  (200 ms to 5 s, `orr.qkc_link.disconnected — reconnecting`). Frames still
  queued when the session breaks are dropped; the DKMS generator re-emits.
- **The SDN is unreachable**: modes 0 and 1 keep working; the announce loop
  retries with backoff (2 s, doubling up to `sdn_announce_secs`). Peers
  already known stay known. The gRPC client is dialled once at boot
  (`SdnClient::connect_opt`); if that fails, `-1` / `>= 2` need the
  `orr_path` hint until a restart.
- **A peer ORR restarts**: it regenerates its ML-KEM key and loses every
  secret. This side notices at its next rotation attempt if it is the
  initiator (a restarted initiator re-runs its own bootstrap at boot) or, in
  the onion modes, at the next frame with an epoch it lacks, and
  re-bootstraps; the peer's new identity is verified against its
  certificate, which did not change.
- **This ORR restarts**: all state is RAM-only and zeroised on drop. It
  re-announces, re-fetches peers, bootstraps the peers it initiates for, and
  the others re-bootstrap it as above.

## Deployment

### Minimal `node.yml`

Derived from [`docker/examples/node.orr.yml`](../docker/examples/node.orr.yml);
the container's [`render_config.py`](../docker/render_config.py) turns it into
the binary's TOML.

```yaml
orr_id: "orr_1"                      # required; convention: orr_<node number>
qkc_id: 1                            # required; the QKC of this node
served_dkms: ["dkms-1"]              # cert identities allowed on SendMessage/StreamDeliveries
qkc_addr: "127.0.0.1:20001"          # optional; default 127.0.0.1:20001 (the QKC's local port)
sdn_url: "https://10.0.0.100:19000"  # SDN gRPC port; the HTTP admin (19002) it announces on is derived
                                     # from it. The gRPC channel itself is only used by the onion modes
                                     # and does not connect under control_tls (see Ports)
advertise_ip: "10.0.0.11"            # the IP the SDN and peer ORRs reach this ORR on;
                                     # needed to announce (the gRPC binds 0.0.0.0)

# peers: { orr_2: 2 }                            # optional seed: orr_id -> qkc_id
# peer_grpc_addrs: { orr_2: "http://10.0.0.12:20003" }  # optional seed: orr_id -> gRPC URL
# sdn_announce_secs: 30                          # heartbeat period
# default_max_hops: 0                            # for clients that send no max_hops
# grpc_tls: true                                 # mTLS on 20003; false only with orr_tls: false on the DKMS
# certs_dir: "/config/certs"                     # where the compose mounts ./certs
# cert_name: "orr_1"                             # default: orr_id
# bootstrap_trust: strict                        # or tofu
# rotation_period_ms: 3600000
# control_tls: true                              # scheme of a bare sdn_url; false = http://
# listen_ip: "0.0.0.0"
# ports: { grpc: 20003, metrics: 20004 }
```

`served_dkms` is optional only in form: without it (and without a `node_id`
in the same file to derive it from) the renderer warns and the ORR accepts
any network-CA certificate on its application surface. `sdn_url` is optional
in `node.yml`: without it the renderer emits no `sdn_http_url`, nothing
announces this ORR and its peers must be seeded by hand.

### Ports

| Port | Default | Protocol | Who connects | TLS |
|---|---|---|---|---|
| gRPC (`grpc_addr`) | 20003 | gRPC `OrrControl` | its DKMS; every peer ORR (bootstrap, rotation) | mTLS by default (`grpc_tls`); client cert from the network CA required |
| metrics (`metrics_addr`) | 20004 | HTTP | Prometheus, optional (no counter is registered today: `/metrics` is empty) | none |

Outbound: the QKC's local port (20001, plain TCP, same host or trusted
network), the SDN HTTP admin (19002, `https://` with `control_tls`, the
default), the SDN gRPC (19000, only for multi-hop paths and topology events;
[`src/sdn_client.rs`](src/sdn_client.rs) dials it without TLS, so with the
default `https://` `sdn_url` that connection fails and the ORR runs without
it) and every peer ORR's 20003. See [ports: who connects to
whom](../docker/README.md#ports-who-connects-to-whom).

### Certificates

| File (`certs_dir`) | Made by | What it authenticates |
|---|---|---|
| `<orr_id>.crt` / `.key` | `docker/gen-certs.sh <orr_id> <advertise_ip> ./certs` (ML-DSA-65, SAN `dkms://<orr_id>` + IP) | This ORR as gRPC server to its DKMS and its peers; as client to peers and to the SDN admin; its key signs the `GetPublicKey` announcement |
| `net-ca.crt` | shared across the federation | The DKMS's client cert (`served_dkms`), peer ORRs' certs and announcement chains, the SDN's server cert |

With `grpc_tls` on (default) and no `[tls]` the ORR refuses to start and
prints what it needs; it never falls back to plaintext. Turning it off is a
written decision on both ends (`grpc_tls: false` here, `orr_tls: false` on
the DKMS) and on every ORR at once — see [the ORR gRPC runs under mTLS by
default](../docker/README.md#the-orr-grpc-runs-under-mtls-by-default-grpc_tls).
`bootstrap_trust = strict` with no `[tls]` and no `peer_verify_keys` is
warned at boot: no announcement could be verified.

### What it announces, what comes back

Request: `POST <sdn_http_url>/register/orr` with `{"id": orr_id, "host":
{"id": 0, "ip": advertise_ip, "port": 20003}, "qkc_id": "<n>"}`. Under
`control_tls` the SDN requires the announced `id` to match the client
certificate. Response: `id`, `accepted`, `changed`, `waiting_for`, `reason`
(which the ORR does not read), and `orr_peers: [{orr_id, qkc_id, grpc_url}]`
sorted by `orr_id`. Log on each change: `anunciado a la SDN accepted=…
waiting_for=…`.

### Running without containers

```bash
./scripts/run-orr.sh                 # CONFIG_DIR=orr/config, cargo run --release -p orr
```

Configuration is [`orr/config/default.toml`](config/default.toml) plus an
optional `local.toml` (gitignored) in `CONFIG_DIR`, then environment overrides
`ORR__<key>` with `__` between nesting levels (`ORR__QKC_LOCAL_ADDR`,
`ORR__ROTATION_PERIOD_MS`; nested tables merge field by field). The
`default.toml` in the repo binds 20003/20004 and points at a QKC on
`127.0.0.1:20001`; add a `[tls]` block or set `grpc_tls = false` before it
starts. `RUST_LOG` selects the log level.

Full procedure: [docker/README.md § 3. ORR](../docker/README.md#3-orr) and the
[quick start](../docker/examples/quick_start.md).

## Health and diagnostics

Every 5 s the ORR logs one `orr.state` line and one `orr.peer` line per peer.
The metrics port exists but nothing is registered on it; the counters that
matter are these.

`orr.state`:

| Field | Meaning |
|---|---|
| `me`, `qkc` | this `orr_id` and its `qkc_id` |
| `peers` | ORRs in the directory (node.yml + SDN) |
| `with_bootstrap` | peers with a `bootstrap_secret` |
| `with_master` | peers with at least one `master_secret` epoch |
| `sent` / `send_failed` | `SendMessage` accepted and queued to the QKC link / failed before leaving (a self-delivery counts in neither) |
| `recv` | frames received from the QKC, any destination |
| `delivered` | handed to the local subscribers |
| `relayed` | forwarded to another ORR (multi-hop onion only) |
| `dropped_no_secret` | frames whose `(from, epoch_id)` has no `master_secret` here |
| `peel_failed` | onion layers whose AEAD tag did not verify (or whose inner layer did not decode) |
| `replay_dropped` | valid layers with an already-seen `(session, counter)` |
| `delivery_no_subscriber` | self-addressed `SendMessage`s (destination = this ORR) with no `StreamDeliveries` open |

`orr.peer`: `qkc` (the peer's QKC), `bootstrap` and `master` (booleans),
`epoch` (latest installed), `send_epoch` (the one used to encrypt to that
peer) and `addr` (its gRPC URL; `None` means no re-bootstrap is possible with
that peer).

Healthy: `peers == with_bootstrap == with_master`, `send_failed` and the
three drop counters flat, `delivery_no_subscriber` at 0, and
`orr.stream_deliveries subscribed` once from its DKMS. In mode 0 the drop
counters stay at 0 by construction; they only move when onion layers are in
use.

Symptoms an operator will meet:

- **`dropped_no_secret` climbing, `orr.incoming master_secret missing for
  epoch (drop)`** (the line is throttled: it appears at powers of two of the
  count): a peer restarted and regenerated its identity. A burst
  that stops is the re-bootstrap converging; one that does not stop means
  the re-bootstrap cannot run — look at that peer's `addr` in `orr.peer`
  and at `orr.passive_rebootstrap: no sé la URL gRPC`. Background in the
  [gotchas](../docs/engineering-notes.md#active-known-issues--gotchas) (ORR
  bootstrap race on a single-ORR restart).
- **`peel_failed` climbing right after `orr.rotation committed`**, with
  `orr.incoming handle_failed error=aead`: frames are arriving with the
  wrong `epoch_id`, i.e. something on the path is not propagating the field
  ([gotchas](../docs/engineering-notes.md#active-known-issues--gotchas), "the
  QKC must propagate `frame.epoch_id`").
- **`peel_failed` climbing on one pair only, steadily and not just after a
  rotation**, every other pair fine, while both ORRs report `master=true`
  for each other: the two ends hold different secrets (the AEAD tag fails,
  so nothing reaches the DKMS). This is the signature of two concurrent
  establishments on the same pair, which the in-flight guard exists to
  prevent
  ([gotchas](../docs/engineering-notes.md#active-known-issues--gotchas), "two
  concurrent `encap`"). With `max_hops = 0` the ORR secret is not on the
  data path, so a `recv_corrupt` on one DKMS pair then points at the DKMS
  e2e layer instead.
- **`accepted=false` forever in `anunciado a la SDN`**: `waiting_for` names
  a QKC that has not registered; if the SDN's response also carries a
  `reason` (the ORR does not log it; the SDN warns `two ORRs on the same
  QKC`), another ORR already holds that QKC.
- **Silent restarts**: with `panic = "abort"` a panic restarts the container
  without `docker ps` showing it. Check `docker inspect` `.RestartCount` and
  grep the log for `panicked at` (`tests/testbed/t00_health.sh` does both).

The DKMS-side view of the same cycle is in [diagnosing the DKMS-to-DKMS key
cycle](../docker/README.md#diagnosing-the-dkms-to-dkms-key-cycle).

## Where things live

| File | Responsibility |
|---|---|
| [`src/service.rs`](src/service.rs) | `OrrService`: mode dispatch, frame build, incoming pump, delivery broadcast, passive re-bootstrap |
| [`src/grpc_server.rs`](src/grpc_server.rs) | `OrrControl` server: `served_dkms` gate, `from`-to-cert binding, mTLS listener |
| [`src/grpc_tls.rs`](src/grpc_tls.rs) | Server identity and client channels to peers; `http://` to `https://` rewrite |
| [`src/qkc_link.rs`](src/qkc_link.rs) | Persistent TCP connection to the QKC's local port, reconnect supervisor |
| [`src/header.rs`](src/header.rs) | `OrrHeader` in `header_orr_mp`: routing ids, `key_id`, `max_hops`, `session`/`counter`, `tag` |
| [`src/onion.rs`](src/onion.rs) / [`src/onion_replay.rs`](src/onion_replay.rs) | Layer seal/peel (AES-256-GCM, HKDF from `master_secret`) and the per-origin replay window |
| [`src/identity.rs`](src/identity.rs) | Per-process ML-KEM keypair; announcement signed with the node certificate |
| [`src/bootstrap.rs`](src/bootstrap.rs) | Pubkey fetch and verification, initiator rule, `EstablishSecret`, `rebootstrap` |
| [`src/rotation.rs`](src/rotation.rs) / [`src/macs.rs`](src/macs.rs) | Epoch rotation task and its HMAC-authenticated RPCs |
| [`src/peers.rs`](src/peers.rs) | `PeerRegistry`: `qkc_id`, pubkeys, `bootstrap_secret`, epochs, in-flight guards |
| [`src/sdn_announce.rs`](src/sdn_announce.rs) / [`src/sdn_client.rs`](src/sdn_client.rs) | Announce loop and peer application; gRPC `GetOrrPath` for multi-hop |
| [`src/config.rs`](src/config.rs) / [`src/stats.rs`](src/stats.rs) | `OrrConfig` with every default; counters and the status line |
| [`../proto/orr.proto`](../proto/orr.proto) | The `OrrControl` schema and message-level protocol notes |

The rustdoc covers the rest: `make doc-open`.

## Further reading

- [Architecture](../docs/architecture.md) — the end-to-end flows the ORR takes part in.
- [Auto-configuration](../docs/auto-configuration.md) — announcements, anchors, peer sets.
- [IPC](../docs/ipc.md) — gRPC control plane and the binary wire the local frames use.
- [Deployment guide § 3. ORR](../docker/README.md#3-orr), [adding a new institution](../docker/README.md#adding-a-new-institution), [deployment.md](../docs/deployment.md).
- Engineering notes: [topology is inferred](../docs/engineering-notes.md#topology-is-inferred-never-configured), [one ORR per QKC](../docs/engineering-notes.md#one-orr-and-one-dkms-per-qkc), [gotchas](../docs/engineering-notes.md#active-known-issues--gotchas) (ORR bootstrap race, concurrent `encap`, `select!` on a `JoinHandle`, `epoch_id` propagation, rotation invariants, mTLS by default, announcements bound to the certificate).
- [SECURITY.md](../docs/SECURITY.md): [Fase 6](../docs/SECURITY.md#fase-6--orrorr-bootstrap-con-pinning--challenge-ejecuta-el-p2-del-backlog-interno-del-orr) (ORR bootstrap trust), [Fase 9](../docs/SECURITY.md#fase-9--intercambio-de-claves-solo-híbrido-rotación-orr-viva-propagación-de-época-hecha-2026-08-30) (rotation and epoch propagation), [Fase 10](../docs/SECURITY.md#fase-10--dos-raíces-de-confianza-por-salto-cert-de-nodo-en-el-qkc-autorización-de-superficies-hecha-2026-08-31) (`served_dkms`).
- [Campaign 2026-09](../docs/results/campaign-2026-09.md) — 60 cells, `peel_failed = 0` throughout; the ORR was never the bottleneck, the star hub's QKC was.
- Testbed: [T30, adding a node](../tests/testbed/README.md#t30--añadir-un-nodo-sin-tocar-los-que-corren-t30_add_nodesh) (hot bootstrap with `strict`), [T32, restarting one module](../tests/testbed/README.md#t32--rearranque-de-un-módulo-suelto-manual-documentado); [local mesh](../tests/local-mesh/README.md).
