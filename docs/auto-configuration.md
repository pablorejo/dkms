# Auto-configuration: how the topology builds itself

There is no topology file. The SDN boots with an empty graph and builds it
from what the modules tell it; the reply to each announcement tells the
module who its peers are. This document says what an operator must declare,
what happens on its own, and how to watch it. The design invariants and the
measurements behind them are in
[engineering-notes.md](engineering-notes.md#topology-is-inferred-never-configured);
the per-module `node.yml` reference is [docker/README.md](../docker/README.md).
The flows that ride on the configured graph are in
[architecture.md](architecture.md); each module's own view of its announce
loop is in [qkc/README.md](../qkc/README.md#the-announce-loop-and-the-peer-set),
[orr/README.md](../orr/README.md#peers-and-the-announce-loop),
[dkms/README.md](../dkms/README.md#sae-bindings-peers-and-demand) and
[sdn/README.md](../sdn/README.md#what-an-announcement-gets-back).

## 1. The principle

Each module of a node makes a periodic `POST /register/<module>` to the SDN's
HTTP admin. The SDN folds the announcement into its graph and answers with
the module's current peer set, derived from that graph. The same POST is the
heartbeat: a module that stops announcing is dropped after a TTL.

```
 node (one institution)                          SDN (operator)
 ┌──────┐ POST /register/qkc {id,host,peer_addr,links[]}    ┌────────────────────┐
 │ QKC  │ ────────────────────────────────────────────────▶ │ graph              │
 │      │ ◀──────────────────────────────────────────────── │  QKC ─ QKC  (edge) │
 └──────┘ {changed, edges_added|pending|conflict|removed,   │  ORR  → QKC        │
           peers[]}                                         │                    │
 ┌──────┐ POST /register/orr {id,host,qkc_id}               │  DKMS → ORR        │
 │ ORR  │ ────────────────────────────────────────────────▶ │  SAE  → DKMS       │
 │      │ ◀──────────────────────────────────────────────── │ presence (TTL)     │
 └──────┘ {accepted, waiting_for, orr_peers[]}              │ measured rates     │
 ┌──────┐ POST /register/dkms {id,host,peer_addr,orr_id,    └─────────┬──────────┘
 │ DKMS │                      saes[]}                                │ on version bump or
 │      │ ────────────────────────────────────────────────▶           │ rate recompute: POST
 └──────┘ ◀──────────────────────────────────────────────── ◀─────────┘ /forwarding-table
          {accepted, waiting_for, dkms_peers[]}                        to every QKC
```

The quditto simulator does not announce anything: it is a KME that both QKCs
of a QKD link reach through their `kme_url`.

## 2. What each module announces

| Module | Endpoint | Payload | Source |
|---|---|---|---|
| QKC | `POST /register/qkc` | `id` (the `qkc_id`; must parse as a number), `host {id, ip, port}` (admin HTTP, port 20002), `peer_addr` (`ip:20000`), `links[]` | [qkc/src/sdn_client.rs](../qkc/src/sdn_client.rs) |
| ORR | `POST /register/orr` | `id`, `host {id, ip, port}` (gRPC, port 20003), `qkc_id` | [orr/src/sdn_announce.rs](../orr/src/sdn_announce.rs) |
| DKMS | `POST /register/dkms` | `id`, `host {id, ip, port}` (SAE port 20005), `peer_addr` (`ip:20006`), `orr_id`, `saes[]` | [dkms/src/southbound/sdn_announce.rs](../dkms/src/southbound/sdn_announce.rs) |

Each QKC `links[]` entry carries `neighbor_id`, `link_type` (`qkd` or `pqc`)
and, when declared in `node.yml`, the physical model the SDN sizes the edge
with: `r0_keys_per_second`, `alpha`, `distance_km` (QKD) or
`pqc_capacity_keys_per_s` (PQC). The QKC does not use these values itself.
Missing fields take the SDN's `EdgeMeta` defaults: `r0` 2000, `alpha` 0.2,
`distance_km` 0, `pqc_capacity_keys_per_s` 10 000. `key_size_bits` is not
announced at all, so the edge always carries the SDN default, 256. Since
the QKC measures its QKD links in situ, every heartbeat also refreshes
`measured_rate_keys_per_s`, `measured_quality` (`measured`, `floor`,
`unavailable`) and `measured_age_ms`; those are the only fields that change
between heartbeats. The DKMS `saes[]` is the sorted list of `sae_bindings`
entries whose value is its own `node_id`.

`host.ip` is `advertise_ip`, or the bind address when it is not `0.0.0.0`.
A module that cannot work out a routable IP logs an error and runs without
announcing; the renderer requires `advertise_ip` for the DKMS.

**Where.** The SDN HTTP admin, `http_addr`, port 19002 by default. The QKC
takes it as `sdn_url`; for the ORR and the DKMS the renderer derives
`sdn_http_url` from the gRPC `sdn_url` / `sdn_endpoint` by swapping the port
and keeping the scheme. With `control_tls` (the renderer's default) a URL
without scheme is rendered `https://`, the announcement is mTLS with the
node certificate, and the SDN accepts it only if the certificate SAN matches
the announced id: `qkc-<id>` for a QKC, the `orr_id`, the DKMS `node_id`
(`require_identity` in [sdn/src/http_api.rs](../sdn/src/http_api.rs); a
mismatch is a 403, logged as `sdn: identity mismatch, rejecting`).
Plaintext is the opt-out, `control_tls: false` on every node at once
([docker/README.md](../docker/README.md#security-and-firewall)). The SDN
validates every announcement before touching the graph: ids are 1 to 64
bytes of `[A-Za-z0-9._:@+-]`, `host.ip` must parse as an IP, `peer_addr` as
`ip:port`, at most 256 links per QKC, rates and distances within range;
anything else is 400.

**How often.** `sdn_announce_secs`, default 30 in all three modules. While
something has not converged (SDN unreachable, `edges_pending` non-empty, or
`accepted: false`) the loop retries every 2 s, doubling up to the period.
The announcers log only when the outcome changes, not on every heartbeat.

**Presence.** The SDN records the last announcement per entity in
[sdn/src/presence.rs](../sdn/src/presence.rs), outside the topology snapshot
so that a heartbeat is not a change. `presence_ttl_secs` (SDN `node.yml`,
default 90 = three missed announcements; `0` disables expiry) is checked by
a sweeper every `presence_ttl_secs / 3`; an entity silent for longer is
deleted, a QKC with its edges. Presence tracks QKCs, ORRs and DKMSs, the
only things that announce (the SDN has no other registration path); SAEs go
with their DKMS. The same sweep evicts `POST /demand` reports older than
`demand_ttl_secs` (default: the presence TTL; `0` = never), so a DKMS that
stops reporting a peer does not leave a ghost commodity in the rate solver.

## 3. What the SDN derives

The graph ([sdn/src/topology.rs](../sdn/src/topology.rs)) has QKCs as
nodes, ORRs anchored to a QKC, DKMSs anchored to an ORR, and SAEs anchored to
a DKMS. `Topology::declared` remembers, per QKC, the links its last
announcement listed; edge `(a, b)` is in the graph when `a` declares `b` or
`b` declares `a`, and both QKCs are registered. Four properties hold this
together:

1. **Announcements are idempotent.** An unchanged announcement does not bump
   `topology.version`; every bump re-pushes forwarding tables and invalidates
   the ORR and DKMS caches, so a heartbeat that looked like a change would
   thrash the network.
2. **A missing anchor is "not yet", not an error.** An edge needs both QKCs,
   an ORR needs its QKC, a DKMS needs its ORR. The reply says so
   (`edges_pending`, `accepted: false` + `waiting_for`), the announcer keeps
   retrying, and any boot order converges. A declaration against a QKC that
   has not registered is stored and closed the moment that QKC announces,
   from either side.
3. **Conflicting link metadata keeps the existing value.** When both ends
   declare the same link with different metadata (`r0`, `alpha`,
   `distance_km`, `link_type`, `pqc_capacity_keys_per_s`), the SDN keeps
   what it has, lists the neighbour in `edges_conflict` and logs it.
   Last-write-wins would make two disagreeing ends overwrite each other on
   every heartbeat. A single declarer is not a conflict: its changes apply.
4. **A QKC's announcement is authoritative over the links it declares.** The
   list replaces the previous declaration wholesale, so dropping a neighbour
   from `node.yml` retires the edge when the other end does not declare it
   either. One declaring end is enough in both directions: a QKC booting
   with nothing declared cannot erase what its neighbour declares, and
   retiring your own link does not depend on the neighbour. This replaced a
   purely additive model in which the SDN handed a QKC back the link the
   operator had just removed
   ([engineering-notes.md](engineering-notes.md#topology-is-inferred-never-configured)).

**One ORR and one DKMS per QKC.** The anchor indexes are 1:1. A second ORR
announcing itself on an occupied QKC, or a second DKMS resolving to the same
QKC through its ORR, is rejected before anything is touched: the reply is
`accepted: false` with a `reason`, the SDN warns once per
(QKC, holder, rejected) triple, and the announcer keeps retrying; it gets in
if the first one expires or moves. Moving an ORR to another QKC, or a DKMS
to another ORR, is supported and clears the index of the anchor it left. The
measured failure that led to the rejection is in
[engineering-notes.md](engineering-notes.md#one-orr-and-one-dkms-per-qkc).

**SAEs.** The DKMS announcement registers its `saes[]`, but never takes an
SAE that another DKMS already owns: those come back in `sae_conflicts` and
stay with their owner until released through `PUT`/`DELETE /sae/<id>` or
expiry.

## 4. What comes back

| Module | Status fields | Peer list | Per entry | Who is listed |
|---|---|---|---|---|
| QKC | `changed`, `edges_added`, `edges_pending`, `edges_removed`, `edges_conflict` | `peers[]` | `qkc_id`, `peer_addr`, `link_type`, `key_size_bits` | graph neighbours that announced a `peer_addr` (one without it is omitted with a warning) |
| ORR | `accepted`, `reason`, `waiting_for`, `changed` | `orr_peers[]` | `orr_id`, `qkc_id`, `grpc_url` | every other ORR: an ORR delivers to any other over the QKC substrate |
| DKMS | `accepted`, `reason`, `waiting_for`, `changed`, `sae_conflicts` | `dkms_peers[]` | `dkms_id`, `orr_id`, `endpoint` | every other DKMS with a `peer_addr` |

All three lists are sorted by id. Each module diffs the list against what it
holds; an unstable order would read as a change on every heartbeat.

What a module does with the list is bounded by four rules:

- **`node.yml` is a floor, not a snapshot.** A link or peer declared locally
  is never removed by the SDN, which can only remove what it added. The ORR
  also keeps its local `peer_grpc_addrs` URL over the one the SDN sends.
  Without this rule, every boot race in which the SDN is behind would tear
  down live links and their key material
  ([engineering-notes.md](engineering-notes.md#peers-ride-back-on-the-announcement)).
  An existing link is left alone: rebuilding it would drop its
  `SecretStore`. Tearing one down wipes that store (its epochs are
  zeroised).
- **Only PQC links are created from the SDN.** A QKD link needs a `kme_url`
  pointing at that institution's own KME, which the SDN cannot know; if the
  SDN offers a QKD link with no local configuration the QKC warns and does
  not create it. A PQC link declared in `node.yml` by `neighbor_id` alone is
  built when the reply supplies the address, with its local configuration
  (suite, rotation, any per-pair material) intact.
- **What a module announces is not what its peers dial.** The QKC announces
  its admin HTTP (20002, where the SDN pushes forwarding) but neighbours
  connect to 20000; the DKMS announces its SAE port (20005) but peers use
  20006. Hence `peer_addr`. The ORR announces the gRPC port peers dial. The
  SDN hands ORR URLs as `http://`; the ORR's own `grpc_tls` (default on)
  rewrites the scheme, and the DKMS prefixes `https://` to peer endpoints.
- **With `[tls]`, a PQC link created from the SDN needs no per-pair secret.**
  Such a link gets no `link_psk` and no `peer_verify_key`; its handshake
  defaults to `pqc_auth = sign`, signed with the node certificate and
  verified against the network CA and SAN. A mode that needs per-pair
  material (`prefer`/`require`, or `sign` without `[tls]`) makes the QKC
  warn that the handshake will be dropped: declare the link in both
  `node.yml` files instead.

The DKMS keeps a peer's local policy (`max_hops`, `security_level`, `sni`,
`orr_path`) and takes `endpoint` and `orr_id` from the SDN
([dkms/src/peers.rs](../dkms/src/peers.rs)). The ORR starts a bootstrap task
for each new peer and forgets the material of one that leaves the list. The
lists are capped: 64 SDN-created links per QKC, 1024 peers per ORR or DKMS.

## 5. What is recomputed on a change

- **Forwarding tables.** A watcher in [sdn/src/service.rs](../sdn/src/service.rs)
  polls `topology.version` and the published rate snapshot every 200 ms. On
  a version bump, and after every rate recompute, it assembles, per QKC, a
  per-destination table: the WCMP next hops of the last recompute (derived
  from the topology and the edge capacities by `wcmp_from_topology`), with
  the shortest path in the current graph as fallback, and POSTs it to
  `<host>/forwarding-table` of every QKC
  (`{"replace": {dst: [{qkc_id, weight}, ...]}}`), all in parallel. The push
  is marked done only when every QKC accepted; otherwise it retries at the
  next tick. With `[tls]` the SDN pushes over `https` with its own
  certificate, and a QKC whose admin runs mTLS accepts the push only from
  the SAN `sdn` ([qkc/src/http_admin.rs](../qkc/src/http_admin.rs)).
  The bump is also broadcast as a `TopologyEvent` on the gRPC
  `StreamTopology`: the DKMS clears its SAE-binding cache, and the ORR its
  path cache when its optional channel is up (it is not under
  `control_tls`, see [ipc.md](ipc.md#tls-on-the-grpc-planes)).
- **Rates.** Recomputed every `mcf_period_ms` (default 5000) by the `num`
  allocator ([engineering-notes.md](engineering-notes.md#solver--fairness));
  a demand report is stored and picked up by the next tick, and the
  `push_debounce_ms` debouncer (100) only applies to the `lp` allocator. A
  version bump does not trigger a recompute either: the next tick sees the
  new graph. The DKMS polls `GET /rate/<dkms_id>` every
  `generator.rate_refresh_ms` (1000) and sends `POST /demand` every
  `demand_refresh_ms` (1000). Routing does not depend on rates: a route
  changes at topology cadence, not every 5 s.
- **Measured link rates.** Each end's `measured_rate_keys_per_s` lands in a
  side registry (`TopologyStore::measured`), outside the snapshot, so noise
  never bumps the version. The minimum of the two ends (an end reporting
  `unavailable` does not vote) is written into
  `EdgeMeta::measured_keys_per_s` only when it differs from the current
  capacity by at least 5 % of it (0.5 keys/s at least); that write is a real
  change and triggers the push, and the next rate tick uses the new
  capacity. From then on it overrides the `r0·10^(-α·d/10)` formula, which
  remains the cold-start prior. PQC edges are ignored, and a heartbeat
  without a measurement (a restarted QKC whose estimator is still cold)
  keeps the applied value
  ([engineering-notes.md](engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01)).

## 6. Lifecycle walk-throughs

### Boot in any order

1. A QKC is accepted as soon as it announces; it has no anchor. Links whose
   neighbour has not registered come back in `edges_pending`.
2. The ORR announces and gets `accepted: false, waiting_for: <qkc_id>` until
   its QKC is in; it retries at 2, 4, 8, 16 s, then every 30 s.
3. The DKMS does the same against its `orr_id`.
4. When the neighbour QKC announces, the pending edge closes in that same
   request; nobody waits for the next heartbeat.

With the three modules of a node up, the node is in the graph within a few
seconds. The other nodes learn about it at their next heartbeat, at most
`sdn_announce_secs` later.

### Adding an institution

Nothing running is touched: no file on any existing node changes.

1. Deploy the new node with its `node.yml` files; the QKC declares its
   physical neighbours, the ORR and DKMS need no `peers` at all
   ([docker/README.md](../docker/README.md#adding-a-new-institution)).
2. It announces; the SDN adds it and bumps the version. Forwarding tables
   are re-pushed to every QKC at the watcher's next tick (200 ms).
3. At their next heartbeat (about 30 s) the graph neighbours' QKCs create
   the PQC link, every ORR starts a bootstrap with the new ORR, and every
   DKMS adds the new peer and starts filling its buffers.
4. Until step 3 has run on a peer, that peer's DKMS answers the newcomer
   with 403 on its ETSI-020 plane (`UnknownPeer`: a valid network
   certificate is not enough, membership in the peer registry is required)
   and drops ORR deliveries from it without touching state.

Verified on the Proxmox testbed
([tests/testbed/README.md](../tests/testbed/README.md), T30, 34 of 35
checks on 2026-08-31: node D joins with one declared link, fills its
buffers with the three existing nodes without corruption, and the other
nine `node.yml` files are byte-identical afterwards; the end-to-end SAE
exchange over the new multi-hop path was the check that did not pass in
that run).

### Removing a node

1. Stop its three modules.
2. After `presence_ttl_secs` (90 s, checked every 30 s, so 90 to 120 s) the
   SDN deletes each entity independently; deleting the QKC removes its edges.
   What other QKCs declared towards it is kept, so the edges re-form on their
   own if it comes back.
3. At the next heartbeat the neighbours drop the links the SDN had created
   (their material is wiped), the ORRs forget its material, the DKMSs drop
   the peer. Links and peers declared locally stay (testbed T31: the
   neighbour drops the link to the removed node and keeps its own).

### Rewiring a link

Edit `links` in the `node.yml` of one QKC and restart that container. Its
next announcement replaces its declaration: a new neighbour becomes an edge
as soon as both ends are registered, a removed neighbour is dropped from the
graph if the other end does not declare it either. A link declared at both
ends therefore has to be removed at both. Exercised on the local mesh with
`mesh.sh link <n> [neighbours...]`
([tests/local-mesh/README.md](../tests/local-mesh/README.md#changing-the-topology-live)).

### Restarts

| Restarted | What heals it |
|---|---|
| QKC | It re-announces within seconds and gets its peer list back; the SDN retries the forwarding push until the QKC accepts it, and re-pushes after every rate recompute in any case. PQC epochs resynchronise on the peer's reconnect or on the first undecryptable frame; an idle broken link heals on first traffic. |
| ORR | It re-announces; a peer whose rotation loop finds it without a `bootstrap_secret` re-bootstraps it at its next rotation tick (up to `rotation_period_ms`, 1 h by default). |
| DKMS | Buffers are RAM-only. Its peers see a new `incarnation` in its first refill traffic, drop their half of the material, and the generator refills in about 30 s. The e2e epoch is re-agreed on first emit. |
| SDN | It boots empty; converged modules re-announce at 2, 4, 8, 16, 30 s after noticing the loss, so the graph is back within one period. A QKC's SDN-created links are dropped by the first reply of the empty SDN and rebuilt as the neighbours re-register; links declared in `node.yml` are not affected. |

The mechanisms and the failures they fixed are in
[engineering-notes.md](engineering-notes.md#active-known-issues--gotchas)
(entries "A restarted DKMS is refilled", "A restarted QKC restarts its epoch
numbering", "PQC link recovery after a single-end restart", "ORR bootstrap
race").

## 7. What is not automatic

- **QKD links.** `type: qkd` with the `kme_url` of the institution's own
  KME, in the `node.yml` of both ends. `r0`/`alpha`/`distance_km` are the
  prior until the estimator measures the link
  ([docker/README.md](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator)).
- **Per-pair link secrets.** `link_psk` or `peer_verify_key`, when used, in
  both `node.yml` files; the SDN carries no secrets. With `[tls]` the
  default handshake needs neither
  ([docker/README.md](../docker/README.md#authenticating-the-link-link_psk--frame_auth)).
- **`key_size_bits`** must be the same at both ends of a link. The QKC does
  not announce it, so a link the SDN creates uses the SDN's default (256); a
  link with another size has to be declared in both `node.yml` files.
- **SAE bindings.** `sae_bindings` in the DKMS `node.yml`. With SAE
  authorisation on (the default) every ETSI-014 request is authorised against
  it and an empty list serves nobody (the renderer and the DKMS both warn);
  it is also what the DKMS announces.
- **The SDN address and the node's own identity.** `sdn_url` /
  `sdn_endpoint`, `advertise_ip`, and certificates from the network CA with
  the right SAN ([docker/README.md](../docker/README.md#1-sdn-central-operator),
  [docs/SECURITY.md](SECURITY.md)).
- **Firewall rules** for the ports peers dial
  ([docker/README.md](../docker/README.md#ports-who-connects-to-whom)).

## 8. Observing it

**SDN read-only views** (`GET`, on `http_addr`; with `[tls]` they sit behind
mTLS, and `http_ro_port` in the SDN `node.yml` opens a plaintext mirror on
loopback):

| Path | Returns |
|---|---|
| `/topology` | `version` and the counts `qkcs`, `orrs`, `dkms`, `saes`, `edges` |
| `/qkcs`, `/orrs`, `/dkms`, `/saes` | the registered entities with their `host`, `peer_addr` and anchors |
| `/links` | per edge: `a`, `b`, `link_type`, `r0_keys_per_second`, `alpha`, `distance_km`, `capacity_keys_per_second`, `measured_keys_per_s`, `measured_reports` per end |
| `/wcmp`, `/rate/<dkms_id>`, `/demand`, `/sae/<id>/binding` | forwarding tables, rates handed to one DKMS, demand reports, SAE resolution |

**The `topology.state` line**, logged by the SDN every 5 s: `version`,
`qkcs`, `orrs`, `dkms`, `saes`, `edges`, `declared` (QKCs declaring at least
one link) and `announced` (entities with a live heartbeat; an ORR or a
DKMS counts only once accepted, so in practice `announced = qkcs + orrs +
dkms`). `declared > 0` with `edges = 0` means declared neighbours have not
registered yet. A rejected anchor does not show here: look for
`accepted=false` in the module's `anunciado a la SDN` line or for the SDN's
`two ORRs on the same QKC` warning.

**Announce log lines** (tracing target = module path; quoted verbatim,
most SDN messages are in English and the modules' in Spanish):

| Where | Message | Meaning |
|---|---|---|
| SDN | `qkc registered` (`added`, `removed`, `pending`), `orr registered`, `dkms registered` | announcement changed the graph |
| SDN | `sdn: identity mismatch, rejecting` | certificate SAN does not match the announced id |
| SDN | `link metadata disagrees between endpoints; keeping the existing value` | property 3 |
| SDN | `two ORRs on the same QKC` / `two DKMSs on the same QKC` | second anchor rejected |
| SDN | `módulo caducado: lleva más de un TTL sin anunciarse` | presence expiry |
| SDN | `forwarding push done` (`topo_from`, `topo_to`, `qkcs_ok`, `qkcs_err`), `topology version changed; broadcast` | reaction to a version bump (the push line also appears after every rate recompute, every `mcf_period_ms`) |
| QKC `qkc::sdn_client` | `anunciado a la SDN` (`added`, `pending`), `perdí a la SDN; reintento` | announce outcome changed |
| QKC | `enlace nuevo, dicho por la SDN`, `enlace local montado con la dirección de la SDN` | link created from the reply |
| QKC | `la SDN anuncia un enlace QKD que no tengo configurado` | QKD link offered without local `kme_url` |
| ORR `orr::sdn_announce` | `anunciado a la SDN` (`accepted`, `waiting_for`), `par nuevo: arranco su bootstrap`, `par retirado por la SDN: olvido su material` | peer set applied |
| DKMS `dkms::southbound::sdn_announce`, `dkms::peers` | `anunciado a la SDN`, `peers actualizados por la SDN` (`added`, `removed`) | peer set applied |
| DKMS | `plano peer: cert de red válido pero no es un peer DKMS conocido (403)` | a DKMS not yet in this node's peer registry |
