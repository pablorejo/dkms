# Architecture

dkms_rust distributes symmetric keys between applications at different
institutions over a network of QKD and post-quantum links. Each institution
runs a **node**: three modules that hold a per-peer buffer of key material,
carry that material hop by hop across the links, and serve keys to
applications over ETSI GS QKD 014. A single controller, the **SDN**, learns
the topology from what the nodes announce and tells each node how to forward
and how fast to fill. No node needs to know the network; no key material ever
reaches the SDN, and none of it is ever written to disk.

This document is the map. The module READMEs ([QKC](../qkc/README.md),
[ORR](../orr/README.md), [DKMS](../dkms/README.md), [SDN](../sdn/README.md),
[quditto](../quditto/README.md)) describe each piece; the [engineering
notes](engineering-notes.md) hold the invariants and the measurements behind
them; [SECURITY.md](SECURITY.md) the trust model; [docker/README.md](../docker/README.md)
how to run a node; [the 2026-09 campaign](results/campaign-2026-09.md) what
all of this measures at N = 10 to 100. The rest of the documentation set is
indexed in [docs/README.md](README.md).

## 1. The pieces

A **node** is one institution's deployment: a QKC, an ORR and a DKMS, plus a
quditto when a link has no QKD hardware. Each module is its own binary and
its own container; they can share a host or not. The **SDN** is the one
process that is not part of a node: the operator runs a single instance for
the whole federation. Outside the system sit the **SAEs**, the applications
that ask a DKMS for keys (ETSI GS QKD 014 clients), and the **KMEs**, the
ETSI GS QKD 014 key servers that hand a QKC the raw material of a `qkd`
link: real QKD hardware exposes one, quditto simulates one.

| Module | Role | Listens on (defaults) | Talks to |
|---|---|---|---|
| [QKC](../qkc/README.md) | Hop-by-hop relay between adjacent nodes. Owns the links and their key material: OTP per block, a MAC per frame, the forwarding table. | 20000 peer (binary TCP), 20001 local (binary TCP, loopback), 20002 admin (HTTPS mTLS) | neighbour QKCs, its ORR, the KMEs of its `qkd` links, the SDN |
| [ORR](../orr/README.md) | Relay between the DKMS and the QKC of a node. Optional onion layers between ORRs. | 20003 gRPC (mTLS) | its DKMS, its QKC, the other ORRs, the SDN |
| [DKMS](../dkms/README.md) | The key server. ETSI-014 to its SAEs, ETSI-020 to the other DKMSs, the per-peer transport buffers, the end-to-end seal. | 20005 SAE (HTTPS mTLS), 20006 peer (HTTPS mTLS), 20007 control (gRPC, loopback) | its SAEs, the other DKMSs, its ORR, the SDN |
| [quditto](../quditto/README.md) | Simulated KME for one link: serves ETSI-014 to the two QKCs of that link at `r0 · 10^(−alpha·distance_km/10)` keys/s. | 20010 (HTTPS mTLS) | the two QKCs of the link |
| [SDN](../sdn/README.md) | The controller: topology graph from announcements, forwarding tables, rates. Sees ids, addresses, capacities and buffer levels; never key material. | 19000 gRPC (mTLS), 19002 HTTP admin (mTLS) | every QKC, ORR and DKMS |

Two institutions and the SDN, with the protocol of every edge:

```
 institution A                                             institution B

 SAE ─ETSI-014 mTLS─► DKMS_A ◄══════ ETSI-020 mTLS ══════► DKMS_B ◄─ETSI-014 mTLS─ SAE
                        │ gRPC mTLS                          │ gRPC mTLS
                      ORR_A ◄──────── gRPC mTLS ─────────► ORR_B
                        │ binary TCP, loopback               │ binary TCP, loopback
 KME ◄─ETSI-014 mTLS─ QKC_A ◄═ binary TCP: OTP+link MAC ═► QKC_B ─ETSI-014 mTLS─► KME
                        │                                    │
                        │  HTTP admin mTLS: announce (QKC, ORR, DKMS)
                        │  HTTPS mTLS: forwarding push (SDN → QKC)
                        │  gRPC mTLS: SAE bindings, ORR paths, topology events
                        └────────────────► SDN ◄─────────────┘
```

The double lines are the two planes that cross institutions with key
material inside: ETSI-020 between DKMSs (session keys, ACKs, the seal's key
agreement) and the binary TCP link between QKCs (transport keys, OTP-encrypted
hop by hop). The ORR-to-ORR gRPC carries only the ML-KEM bootstrap and
rotation of the onion secret; the SDN edges carry no material at all. On a
`qkd` link both QKCs obtain the same keys from the KME of that link: the
sender calls `enc_keys`, tells the receiver the `key_ID`s in a NOTIFY frame,
and the receiver calls `dec_keys`. A `pqc` link needs no KME: the two QKCs
agree an ML-KEM secret over their own TCP connection and derive the material
from it ([qkc/src/pqc_source.rs](../qkc/src/pqc_source.rs)). Ports and
firewall rules: [docker/README.md § Ports](../docker/README.md#ports-who-connects-to-whom).

## 2. Two kinds of keys

The system moves two different kinds of key material on two different paths.

| | Transport key | Session key |
|---|---|---|
| What | 32 random bytes with a UUIDv4 `key_id` (`generator.key_size_bytes`). | 64 to 256 random bits with a UUIDv4 `key_ID`: what a SAE receives from ETSI-014 (`size` in the request; message types in [etsi/README.md](../etsi/README.md)). |
| Made by | The generator of the sending DKMS, continuously, at the rate the SDN hands that pair ([dkms/src/control/generator.rs](../dkms/src/control/generator.rs)). | The master's DKMS, on `enc_keys`, one request at a time ([dkms/src/service.rs](../dkms/src/service.rs)). |
| Travels | DKMS → ORR → QKC → … → QKC → ORR → DKMS, sealed end to end by the DKMS and OTP-encrypted on every link. | DKMS → DKMS in one ETSI-020 `ext_keys` request, XOR-wrapped with one transport key. |
| Stored | `buffer_enc[peer]` at the sender and its twin `buffer_dec[sender]` at the receiver: two FIFOs per peer of `buffer.capacity_per_peer` (4096) keys, split by grade on the ENC side ([dkms/src/state/pool.rs](../dkms/src/state/pool.rs)). | The pending store of each DKMS until every authorised SAE has collected it or `ttl_seconds` expires ([dkms/src/state/pending.rs](../dkms/src/state/pending.rs)). |
| Consumed | One per session key per destination DKMS. One-shot: once its envelope has left the process the key is spent and the bytes are zeroised (a key popped for an envelope that never left goes back to the front of its buffer). | Returned to the master in the `enc_keys` response, collected once per slave SAE with `dec_keys`. |
| Grade | `qkd` if the SDN's `/rate` reports a QKD-only path to that peer (`qkd_available`, assumed until the first answer arrives), `pqc` otherwise. | Inherits the grade of the transport key that wrapped it; the request's `security_level` (`strict_qkd`, `qkd_prefer`, `no_worry`) decides which grade may serve it ([common/src/security.rs](../common/src/security.rs)). |

Why two. The QKC network delivers 32-byte blocks hop by hop at whatever rate
the links sustain, with the latency of a multi-hop path and no notion of a
request. SAE traffic is bursty and synchronous. Filling the transport buffers
ahead of demand decouples the two: `enc_keys` is served from RAM at the cost
of one HTTPS round trip to the peer DKMS, the link layer never sees an
application request, and the SDN can allocate the fill as a continuous rate
per pair. The transport key is then the one-time pad of the session key, so
the ETSI-020 leg is information-theoretically wrapped even though it runs
inside TLS, and a key's grade is exactly the grade of the path its pad took.

## 3. Flow 1: filling the transport buffers

This is the background flow: it runs whenever a buffer has headroom, whether
or not any SAE is asking. DKMS_A fills `buffer_enc[B]` on its side and
`buffer_dec[A]` on DKMS_B with the same 32-byte keys, down through ORR_A and
QKC_A, across the links, and up to DKMS_B.

1. **Rate.** Every `generator.rate_refresh_ms` (1 s) the generator of DKMS_A
   polls `GET /rate/<dkms_id>` on the SDN's HTTP admin and gets, per peer, a
   fill rate in keys/s plus `qkd_available`. The rate feeds one token bucket
   per peer; a bucket never yields more than
   `max_tokens_per_peer_per_tick` (32) per `tick_ms` (100 ms), so 320 keys/s
   per pair is the hard ceiling whatever the SDN says. Where the SDN gets
   the number: [section 6](#6-control-plane).
2. **Emit.** Each tick the generator takes a batch per peer, capped to the
   headroom `capacity_per_peer − (enc + ack_pending)`, and emits to every
   peer concurrently, at most `max_emits_in_flight` (128) in flight. Each
   key is 32 bytes from the process CSPRNG (`rand::thread_rng`) with a
   UUIDv4 `key_id`; it goes into `ack_pending[B]` with a deadline of
   `ack_timeout_ms` (30 s). Emitting one
   peer at a time made the per-peer rate fall as O(1/N); the bound is the
   only back-pressure on this path ([engineering notes](engineering-notes.md#active-known-issues--gotchas),
   "The generator emits to every peer at once").
3. **Seal (DKMS_A).** `E2e::seal` encrypts the 32 bytes with AES-256-GCM
   under `master[epoch]`, a secret DKMS_A and DKMS_B agreed with ML-KEM
   over their ETSI-020 mTLS channel (`POST /kmapi/v1/e2e/kem` on port
   20006). Key and nonce are HKDF-derived from the secret and the `key_id`,
   so each key meets exactly one nonce. The tag, the epoch and a per-pair
   counter ride in the DKMS header (`e2e_tag`, `e2e_epoch`, `e2e_ctr`), never
   in the payload, which stays 32 bytes. The AAD covers the whole DKMS
   header (`msg_type=DKMS_BUFFER`, `key_id`, `key_size_bits`, `sae_origin`,
   `timestamp_ms`, `request_id`, `incarnation`, `e2e_epoch`, `e2e_ctr`:
   everything but the tag) plus origin and destination. If DKMS_A holds no
   epoch for B yet, the emit fails, an agreement is requested in the
   background (at most one attempt per 2 s per peer) and the next tick
   retries. Design in
   [dkms/src/e2e.rs](../dkms/src/e2e.rs).
4. **ORR_A.** DKMS_A calls `SendMessage(destination = orr_B, payload,
   app_header, max_hops = 0, grade)` over gRPC mTLS
   ([proto/orr.proto](../proto/orr.proto)). ORR_A accepts the call only
   from an identity in `served_dkms`, maps `orr_B` to `qkc_B` through its
   peer directory, and writes a `FRAME_LOCAL_SEND` with
   `dest_final = qkc_B` and the DKMS header untouched in `header_dkms_mp` to
   QKC_A's local port over loopback. With `max_hops = 0` there is no ORR
   layer; the onion modes would add theirs here
   ([orr/src/onion.rs](../orr/src/onion.rs)).
5. **QKC_A.** `handle_local_send` picks the next hop from the forwarding
   table: a stable hash of the frame selects one of the next hops the SDN
   weighted for `dest_final`, so a flow keeps its path and the load spreads
   ([qkc/src/relay.rs](../qkc/src/relay.rs)). It takes one OTP key per
   block of `key_size_bits / 8` bytes (32 with the node.yml default) from
   the outgoing link's ENC buffer, XORs the payload,
   lists the spent `key_ids` in the frame, then appends the link MAC trailer
   `session ‖ counter ‖ tag` (HMAC-SHA256 over the entire frame: identities,
   both cleartext headers, ciphertext) and sends `FRAME_RECV_AUTH` (last hop)
   or `FRAME_RELAY_AUTH` (transit) on port 20000. On a `qkd` link the keys
   came from the KME (`enc_keys`) and the neighbour was told their ids in a
   `FRAME_KEY_IDS_NOTIFY`, so it fetches them with `dec_keys`; on a `pqc`
   link both ends derive each key from the epoch secret named in the first
   four bytes of the `key_id`. Every payload byte costs key material, stepped
   per block, which is why nothing that is not secret rides in the payload
   ([engineering notes](engineering-notes.md#active-known-issues--gotchas),
   "On an OTP link, every byte added to the payload").
6. **Every receiving QKC.** The connection reader verifies the MAC, then the
   anti-replay window, in arrival order: doing it in concurrent tasks
   rejected legitimate frames under load
   ([engineering notes](engineering-notes.md#active-known-issues--gotchas),
   "The link MAC and the anti-replay window are checked in the connection
   READER"). Then it decrypts with the incoming link's DEC buffer. If
   `dest_final` is itself, it hands its ORR a `FRAME_LOCAL_DELIVER`; otherwise
   it re-encrypts with the outgoing link's keys and forwards. `header_orr_mp`,
   `header_dkms_mp` and `epoch_id` propagate byte for byte.
7. **ORR_B.** Decodes its own header, finds no layer to peel, and broadcasts
   a `DeliveredMessage` (origin, destination, payload, `app_header`) on the
   `StreamDeliveries` stream DKMS_B keeps open.
8. **DKMS_B.** `handle_orr_delivery_buffer` refuses a source that is not a
   registered peer and a payload whose length disagrees with the header,
   then opens the seal **before** reading anything else: an unknown epoch
   triggers a key agreement request and the key is dropped; a repeated
   counter is dropped; a bad tag is counted as `recv_corrupt`, dropped and
   never acknowledged. Only then does it act on `incarnation` (a changed one
   wipes what it holds for that peer, see [section 8](#8-state-and-restarts))
   and push the key into `buffer_dec[A]`.
9. **ACK.** DKMS_B batches the received `key_id`s and POSTs them to
   `https://<A>:20006/kmapi/v1/ext_keys/ack` over ETSI-020 mTLS
   (`ack_transport = etsi020`); the sender identity is DKMS_B's client
   certificate. On DKMS_A, `Generator::on_ack` moves the entry from
   `ack_pending` to `buffer_enc[B]`. An entry that reaches its deadline is
   zeroised and counted as `expired`. The legacy plain-TCP ACK socket
   (port 20009) is off by default (`ack_socket_listen = false`).

What each party can read along the way is in [section 5](#5-the-layers).
The per-peer counters of this whole cycle are the `generator.state` log line
of the DKMS, one per peer every 5 s
([dkms/README.md § Health](../dkms/README.md#health-and-diagnostics)).

## 4. Flow 2: a SAE asks for a key

The foreground flow, one ETSI-014 request at a time: SAE_a at node A wants a
key shared with SAE_b at node B. Handlers in [dkms/src/etsi_http/v014.rs](../dkms/src/etsi_http/v014.rs)
and [v020.rs](../dkms/src/etsi_http/v020.rs); logic in [dkms/src/service.rs](../dkms/src/service.rs).

1. **Request.** SAE_a calls `GET` or `POST /api/v1/keys/<SAE_b>/enc_keys` on
   DKMS_A's SAE port (20005) over mTLS. The master identity is the SAN of the
   client certificate (`urn:dkms:sae:<id>`), never a body field, and it must
   be bound to DKMS_A in its `sae_bindings` (404 `UnknownSae` otherwise;
   `sae_authorization: false` in `node.yml`, rendered as
   `sae.enforce_authorization`, disables the check). The body
   may carry `number`, `size`, `additional_slave_SAE_IDs` and a
   `security_level` extension; limits are those `/status` advertises (at
   most 64 keys and 16 SAEs per request, 64 to 256 bits per key).
2. **Resolve.** Each target SAE is mapped to the DKMS that serves it
   (`GetSaeBinding` on the SDN's gRPC, cached with a TTL; the static
   `sae_bindings` table without an SDN) and the targets are grouped by DKMS.
   A `strict_qkd` request towards a peer the SDN reports as QKD-unreachable
   is refused up front rather than served a PQC-grade key.
3. **Admit.** The demand is recorded for the SDN first, so a rate-limited
   SAE still shows as demand instead of starving itself; then the request is
   charged to a token bucket per (peer, SAE) whose refill is the SDN rate
   shared among the SAEs active on that peer (429 when empty).
4. **Generate.** DKMS_A draws `number` session keys from the OS RNG, each
   with a UUIDv4. Targets served by DKMS_A itself go straight into its
   pending store.
5. **Wrap and send.** For each remote DKMS, `build_ext_keys_envelope` pops
   one transport key per session key from `buffer_enc[peer]`, of the grade
   the security level prefers (503 `transport buffer empty` if there is
   none), XORs the session key with it, and POSTs one ETSI-020
   `ext_keys` container to `https://<peer>:20006/kmapi/v1/ext_keys` with the
   `target_sae_ids` of that DKMS. Each key's extension carries
   `transport_key_id` and `session_key_digest` (the first 16 bytes of
   SHA-256 over `key_ID ‖ key`); `ttl_seconds` (`pending.default_ttl_secs`, 86400)
   is a mandatory extension. Sends to several DKMSs run in parallel, each
   bounded by `request.peer_send_timeout_ms` (1500).
6. **Unwrap (DKMS_B).** The caller must be a registered peer, not just a
   holder of a network certificate. For each key DKMS_B takes the named
   transport key out of `buffer_dec[A]` by id, XORs, checks the digest (a
   mismatch is counted as `recv_corrupt` and not acknowledged: two SAEs
   would otherwise end up with different keys and no error anywhere), and
   stores the session key for the target SAEs, which must be ones DKMS_B
   serves. An existing `key_ID` is never overwritten. The reply is the
   ETSI-020 ack container.
7. **Deliver.** DKMS_A answers SAE_a with the key container in clear.
   SAE_b calls `GET` or `POST /api/v1/keys/<SAE_a>/dec_keys` on DKMS_B with
   the `key_ID`s; the master in the path must be the initiator of the entry.
   Each SAE may collect a `key_ID` once. The entry disappears when every
   authorised SAE has collected it or its TTL expires.
8. **Additional SAEs.** `additional_slave_SAE_IDs` are resolved with the
   slave: the ones on the same DKMS share the envelope, the ones elsewhere
   get their own, and every one of them is authorised on the pending entry.
   One transport key is spent per session key per destination DKMS.
9. **Failure.** If anything fails before a request has left the process
   (empty buffer, unknown peer, no client), the popped transport keys go
   back to the front of their buffer in order, the tokens are refunded and
   the local pending entries are retracted. If a send fails or times out,
   only the failed destinations are refunded: the transport keys that went
   out are spent whether or not the peer received them. The SAE gets 502. A
   peer answering that the transport key is not in its `buffer_dec` means
   DKMS_A's `buffer_enc[peer]` is stale; DKMS_A clears it so the generator
   rebuilds it.

## 5. The layers

Each layer exists because a specific position on the path could otherwise
read, alter or replay something. The two keyed integrity layers (link MAC,
e2e seal) date from 2026-08-28; before them OTP and the ORR's XOR layer were
malleable and the only check was an unkeyed digest inside the encryption
([engineering notes](engineering-notes.md#active-known-issues--gotchas),
"The data path has two keyed integrity layers").

| Layer | Applied by | Protects | Key root | Rotation |
|---|---|---|---|---|
| TLS 1.3 mTLS, `X25519MLKEM768` only, ML-DSA-65 certificates | every module, on every plane that leaves the host except the QKC-to-QKC link, which has its OTP and MAC instead: ETSI-014, ETSI-020, the ORR and SDN gRPC, the SDN and QKC HTTP admin, QKC to KME | the connection: confidentiality, integrity, both identities | `net-ca` (nodes and SDN), `sae-ca` (SAEs), the KME's own PKI | per connection |
| e2e seal, AES-256-GCM ([dkms/src/e2e.rs](../dkms/src/e2e.rs)) | sending DKMS, opened only by the receiving DKMS | the transport key, the whole DKMS header, origin and destination; `(incarnation, counter)` against replay | per-pair ML-KEM secret agreed over ETSI-020 mTLS | epoch every `rekey_secs` (3600, driven by the lex-smaller peer) or `rekey_keys` (100 000, by either) |
| onion layers, AES-256-GCM, optional (`max_hops ≠ 0`, [orr/src/onion.rs](../orr/src/onion.rs)) | ORR, one layer per ORR on the path | path privacy on top of the sealed payload; AAD binds `key_id`, `epoch_id`, `max_hops`, freshness and the DKMS header | per-pair ORR `master_secret`, ML-KEM bootstrap signed with the node certificate | epoch every `rotation_period_ms` (1 h) |
| link OTP ([qkc/src/crypto.rs](../qkc/src/crypto.rs)) | each QKC, per link, one key per block of `key_size_bits / 8` bytes (32 with the node.yml default) | the payload on one link; information-theoretic on a `qkd` link | the KME (`qkd`) or the ML-KEM epoch secret of the link (`pqc`) | every key |
| link MAC, HMAC-SHA256 + replay window ([common/src/crypto/frame_mac.rs](../common/src/crypto/frame_mac.rs), [qkc/src/frame_auth.rs](../qkc/src/frame_auth.rs)) | each QKC, per link, on every data frame and NOTIFY | the entire frame: identities, both cleartext headers, ciphertext; `session ‖ counter` against replay | the link's epoch secret (`frame_auth = require`, the default with a node certificate) or `link_psk` | with the link epoch |
| OTP wrap of the session key ([dkms/src/service.rs](../dkms/src/service.rs)) | master DKMS, unwrapped by the slave DKMS | the session key inside ETSI-020, information-theoretically | one transport key | every key |
| `session_key_digest` | master DKMS, checked by the slave DKMS | that the unwrapped session key is the one the master generated, bound to its `key_ID` | none: unkeyed, inside mTLS | — |

What an attacker at each position sees and can do, with everything at its
defaults:

| Position | Sees | Can | Cannot |
|---|---|---|---|
| The wire between two QKCs (port 20000) | OTP ciphertext; the cleartext frame fields: QKC ids, `key_ids`, `epoch_id`, the ORR and DKMS headers (`key_id`, `sae_origin`, `incarnation`, the seal fields), the MAC trailer | drop or delay frames | read the payload; alter any field (the MAC covers the whole frame); inject or replay (no link root, and the receiver's window) |
| A QKC on the path (another institution's node in transit) | the same fields, and the sealed payload as AES-GCM ciphertext between decrypt and re-encrypt | drop, delay or misroute frames; count keys per pair | read the transport key; alter the payload or the DKMS header (the seal's AAD covers both, plus origin and destination); replay a `DKMS_BUFFER` (e2e counter); forge an `incarnation` to wipe a pair's buffers; move a key to another pair |
| A peer ORR (with `max_hops = 0`, the destination node's own; a transit one in onion modes) | the sealed payload and its headers; in onion modes, one layer less than its predecessor | deliver, forward or drop | open the seal (it belongs to the two DKMSs); speak for another ORR on the gRPC plane (`from` must match its certificate) |
| The network between two DKMSs (port 20006) | TLS 1.3 records | drop or delay | read or alter `ext_keys`, ACKs or the KEM exchange; connect without a `net-ca` certificate that is also a registered peer; and even with TLS broken, a session key is only ever seen XORed with a transport key it does not have |
| The SDN, or whoever holds its certificate | ids, addresses, capacities, buffer levels, demand | choose the path (forwarding tables) and the rates; add or remove `pqc` links | read any key: no material crosses the control plane; the node it routes through still cannot open the seal |

Residual gap, as stated in the [engineering
notes](engineering-notes.md#roadmap-and-measured-state): **the SAE has no
check of its own on the session key**. It trusts its DKMS, as ETSI-014
assumes of a KME. The DKMS-to-DKMS leg is covered by `session_key_digest`,
so a transport key that differs between the two ends is detected at the
slave's DKMS and the key is discarded rather than delivered; the SAE
observes that as a failed `dec_keys`, not as a wrong key.

## 6. Control plane

There is no topology file. Each module makes a periodic `POST
/register/<module>` to the SDN's HTTP admin (every `sdn_announce_secs`, 30 s):
a QKC announces itself and its links with their physical model and the rate
it measures in situ, an ORR the QKC it hangs off, a DKMS its ORR and the SAEs
it serves. The SDN folds that into a graph, drops whatever stays silent for
`presence_ttl_secs` (90 s), and answers every announcement with the module's
current peer set, so a new institution becomes reachable without touching a
running node and any boot order converges. From the graph and the per-link
capacities (the quditto formula for `qkd` until the QKCs' in-situ measurement
overrides it, the declared value for `pqc`) it derives two things it keeps
deliberately apart: **forwarding
tables**, WCMP next hops per destination pushed to every QKC's `POST
/forwarding-table` on every topology bump and on every recompute, and
**rates**, one fill rate per DKMS pair recomputed every `mcf_period_ms` (5 s)
from the demand the DKMSs report on `POST /demand` and served on
`GET /rate/<dkms_id>`. Routes never depend on
buffer levels and rates never move routes; coupling the two through the LP's
edge flows left links dry while others idled
([engineering notes](engineering-notes.md#active-known-issues--gotchas),
"Routing is decoupled from the rate solver"). What each module announces,
what it gets back, and what an operator watches:
[auto-configuration.md](auto-configuration.md). The invariants that keep the
graph stable: [engineering notes](engineering-notes.md#topology-is-inferred-never-configured).
The allocators: [SDN README](../sdn/README.md#rates) and
[engineering notes](engineering-notes.md#solver--fairness).

## 7. Trust roots

Two offline certificate authorities, generated by
[docker/gen-certs.sh](../docker/gen-certs.sh) with ML-DSA-65 keys: **`net-ca`**
signs one certificate per node module and one for the SDN (SAN
`URI:dkms://<id>`: `qkc-<id>`, `orr_<id>`, `dkms-<id>`, `sdn`), **`sae-ca`**
signs SAE client certificates only (SAN `URI:urn:dkms:sae:<id>`), one per
institution if wanted. The node certificate is the root of every inter-module
plane: ETSI-020, DKMS to ORR and ORR to ORR gRPC, the SDN's admin and gRPC,
the forwarding push (the QKC accepts a table only from the `sdn` identity),
the ORR's announced ML-KEM key (signed with it), and the QKC-to-QKC handshake
of both link types (`pqc_auth = sign`, the default once a node has `[tls]`),
from whose epoch secret the `pqc` OTP material and the per-frame MAC root
derive; the per-pair secrets of the e2e seal and the onion are agreed over
channels this certificate authenticates. Identity is always the certificate's
SAN, never a body field, and a mutating request must name the identity that
presents it. The second root is the **KME**: on a `qkd` link the data keys
come from it, the QKC authenticates to it with the credential of the KME's own
PKI (`kme_cert`, `kme_key`, `kme_ca`) or, for a quditto, with the network
certificate, and the link handshake only supplies the MAC root. Every TLS
endpoint speaks TLS 1.3 with the hybrid `X25519MLKEM768` exchange and nothing
else, verified at boot by a self-handshake
([common/src/tls_pqc.rs](../common/src/tls_pqc.rs); the provider and the
shared crypto are described in [common/README.md](../common/README.md)), so
SAE clients need a TLS stack that supports it. The threat model, the plane-by-plane table and
the phases that got here: [SECURITY.md](SECURITY.md#1-alcance-y-modelo-de-amenaza)
and [§ 2 PKI](SECURITY.md#2-pki-objetivo); the operator's view:
[docker/README.md § Security and firewall](../docker/README.md#security-and-firewall).

## 8. State and restarts

No state is written to disk. Key material lives in RAM inside `Zeroizing`
containers and is wiped when dropped; the QKC, ORR, DKMS and SDN binaries
lock their memory out of swap and disable core dumps, best-effort under the
container's limits ([common/src/hardening.rs](../common/src/hardening.rs)). A restart loses state
by design, and each module has a mechanism to converge again without an
operator. Each mechanism is the fix of a measured failure; the entries quoted
below are in the [engineering notes' gotchas](engineering-notes.md#active-known-issues--gotchas).

| State | Owner | After a restart of the owner |
|---|---|---|
| Topology graph, presence, measured rates, demand reports, allocator price state | SDN | Boots empty. Every module re-announces with backoff (2 s upward, 30 s steady), so the graph is back within one period; modules keep the links and peers they hold, and the SDN may only remove the peers it added ([peers ride back on the announcement](engineering-notes.md#peers-ride-back-on-the-announcement)). |
| Forwarding table (`ArcSwap`, written by the SDN's push) | QKC | Empty until the next push. The SDN pushes on every topology bump and on every rate recompute (`mcf_period_ms`, 5 s) and retries until every QKC accepts, so the table is back within seconds. |
| Link key buffers (ENC and DEC per link) and `pqc` epoch secrets | QKC | Empty, and the epoch numbering starts over from 0. On a `qkd` link the buffers refill from the KME. On a `pqc` link the initiator relinks above both windows when it sees the peer reconnect, and a responder that receives epochs it lacks asks for a resync (`FRAME_PQC_RESYNC_REQ`); an idle broken link heals on the first frame sent over it ("A restarted QKC restarts its epoch numbering", "PQC link recovery after a single-end restart"). |
| ML-KEM identity and per-pair `master_secret` epochs | ORR | A new keypair, verified by peers against the unchanged node certificate. A peer whose rotation loop finds "no bootstrap_secret" re-bootstraps it at its next rotation tick (up to `rotation_period_ms` later; a failed attempt retries with backoff up to 30 s), traffic or not ("ORR bootstrap race on a single-ORR restart"). Only the onion modes consume this. |
| `buffer_enc`, `buffer_dec`, `ack_pending`, e2e epochs | DKMS | Empty. Its first refill traffic carries a new `incarnation`; each peer drops its half of the material for that pair and the generator refills in about 30 s. The e2e epoch is re-agreed on first emit; a receiver seeing an unknown epoch asks too ("A restarted DKMS is refilled because its peers notice the restart", "The end-to-end layer on transport material lives in the DKMS"). |
| Pending session keys | DKMS | Lost. A slave SAE that had not collected gets 404 on `dec_keys`; the master requests again. |
| Peer registry, SAE binding cache | DKMS, ORR, QKC | Rebuilt from `node.yml` and the next announce response. |

The convergence sequence for each case, with the log lines to watch, is in
[auto-configuration.md § Restarts](auto-configuration.md#restarts).

## 9. Transports

Two transports, and no third. **gRPC** (tonic, schemas in
[proto/](../proto/)) carries the control plane between modules: `OrrControl`
between a DKMS and its ORR and between ORRs, `SdnControl` from the DKMSs
to the SDN (the ORR's optional channel, used only by the onion modes, does
not come up under `control_tls`: [ipc.md](ipc.md#tls-on-the-grpc-planes)),
`DkmsControl` for the operator. **HTTP** carries what has a
standard or an admin shape: ETSI-014 and ETSI-020 on the DKMS (HTTP/2 over
mTLS), the SDN's admin API, the QKC's admin port, the KME's ETSI-014. **Binary
TCP** ([wire/README.md](../wire/README.md)) carries only the hot path: the
QKC-to-QKC link on port 20000 and the ORR-to-QKC local port 20001, same
frame, the local one in the clear on loopback. A frame is a 10-byte prefix,
fixed-width ids and link fields (the QKC's own header), the list of spent
`key_ids`, two length-prefixed msgpack headers (ORR, DKMS) and the raw
ciphertext: one read and one write per hop, no base64, and the QKC
propagates the two upper headers as opaque bytes.
That path carries one 32-byte block per key, thousands of frames per second
per link, re-encrypted at every hop: its per-frame cost is the system's
throughput. The control plane changes at announce cadence and is not worth a
second wire format. The magic prefix carries a wire version; a mismatch fails
loudly rather than misparse. Frame kinds, field layout and the gRPC dial
defaults: [ipc.md](ipc.md).

## 10. Glossary

| Term | Meaning |
|---|---|
| node | One institution's deployment: its QKC, ORR and DKMS, plus a quditto when a link has no hardware. |
| SDN | The single network controller, run by the operator; builds the graph from announcements, pushes forwarding tables, allocates rates. |
| SAE | Secure Application Entity: the application that asks a DKMS for keys, an ETSI GS QKD 014 client identified by its `sae-ca` certificate. |
| KME | An ETSI GS QKD 014 key server that feeds a `qkd` link; real QKD hardware exposes one, quditto simulates one. |
| link | A QKC-to-QKC connection: type `qkd` (material from a KME) or `pqc` (material derived from an ML-KEM secret the two QKCs negotiate). |
| link key | One OTP key of `key_size_bits` (256 in `node.yml`: 32 bytes, one per block of payload) in a link's buffers: from the KME on a `qkd` link, derived from the epoch secret on a `pqc` link. What a link key encrypts, hop by hop, is a transport key. |
| transport key | A 32-byte key filling `buffer_enc[peer]` on one DKMS and its twin `buffer_dec` on the peer; produced by the generator, carried by ORR and QKCs, sealed end to end by the DKMS. |
| session key | The key a SAE receives from ETSI-014 `enc_keys`; generated by the master's DKMS, wrapped with one transport key, delivered to the slave's DKMS over ETSI-020 `ext_keys`, collected with `dec_keys`. |
| announcement, announce loop | The periodic `POST /register/<module>` each module makes to the SDN's HTTP admin; also its heartbeat, and the channel its peer set comes back on. |
| peer | The module of the same type at another node: a QKC's neighbours, every other ORR, every other DKMS. |
| forwarding table | The per-destination next-hop table (WCMP, several weighted next hops) the SDN pushes to each QKC. |
| epoch | One numbered secret in a rotating series: of a link's handshake (the OTP material on a `pqc` link, the MAC root on both link types), of an ORR pair, of a DKMS pair's e2e seal (drawn at random there). Numbers are never reused with a different secret. |
| link MAC | The per-frame HMAC-SHA256 plus anti-replay window on QKC-to-QKC frames, verified in the connection reader in arrival order. |
| onion | Optional per-hop AES-256-GCM layers the ORR adds when `max_hops ≠ 0`; off by default (`max_hops = 0`, the ORR is a relay). |
| e2e seal | The AES-256-GCM seal the DKMS puts on every transport key (`DKMS_BUFFER`) under a per-pair ML-KEM secret; opened only by the receiving DKMS. |
| grade | `qkd` when the SDN reported a QKD-only path to the peer at the moment the transport key was emitted, `pqc` otherwise (the QKD-only forwarding table that enforces it hop by hop is pushed only with `SDN_DUAL_GRADE_TABLES`, see [qkc/README.md](../qkc/README.md#forwarding-the-wcmp-table)); a session key inherits the grade of the transport key that wrapped it. |
| incarnation | A random id per process run, carried in every `DKMS_BUFFER` header, by which a peer DKMS tells a restart from a replay. |
