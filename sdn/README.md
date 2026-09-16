# SDN — the network controller

The SDN is the one module that is not part of a node: the operator runs a
single instance for the whole federation. Every QKC, ORR and DKMS announces
itself to it, and from those announcements it builds the topology graph,
decides how each QKC forwards frames (multipath forwarding tables) and how
fast each pair of DKMSs may fill its transport-key buffers (rates). It sees
ids, addresses, link parameters, buffer levels and drain rates. It never sees
key material of any kind: no KME keys, no transport keys, no session keys.

## What it does

- **Builds the topology from announcements.** `POST /register/qkc`,
  `POST /register/orr` and `POST /register/dkms` on the HTTP admin
  (`http_addr`, port 19002) are the only way entities enter the graph. There
  is no topology file and `PutTopology` on gRPC answers `UNIMPLEMENTED`
  (`src/topology.rs`, `src/http_api.rs`).
- **Tracks liveness.** The announce loop is the heartbeat; a module silent
  for `presence_ttl_secs` (default 90) is dropped with its edges
  (`src/presence.rs`; the sweeper in `src/service.rs` logs `módulo caducado`).
- **Tells each module who its peers are.** The announce response carries the
  peer set derived from the graph, so a new institution becomes reachable
  without touching any running node (`Topology::{qkc_peers,orr_peers,dkms_peers}`).
- **Models link capacity.** Quditto formula for `qkd` links, declared
  capacity for `pqc` links, overridden by the rate the QKCs measure in situ
  (`EdgeMeta::capacity_keys_per_second`).
- **Computes and pushes forwarding tables.** WCMP tables from topology and
  capacity, pushed to every QKC's `POST /forwarding-table` whenever the
  topology version or the published snapshot changes (log
  `forwarding push done`).
- **Allocates rates.** Every `mcf_period_ms` (default 5000) it recomputes
  the per-pair rates from the demand the DKMSs report on `POST /demand` and
  serves them on `GET /rate/<dkms_id>` (`rate_allocator`, default `num`).
- **Answers the small gRPC surface** the DKMS and ORR need: SAE binding
  lookup, ORR-level paths and a topology event stream (`src/grpc_server.rs`,
  `grpc_addr`, port 19000).
- **Authenticates and authorises the control plane.** With `[tls]` both
  planes run under mTLS and every mutating route binds the announced id to
  the client certificate (`src/mtls.rs` extracts it, `require_identity` in
  `src/http_api.rs` checks it).

## How it works

### The graph and the four properties that keep it stable

The SDN boots with an empty graph. A QKC announces its id, its admin
endpoint (`host`), the address neighbours dial (`peer_addr`) and its `links`
(`neighbor_id` and `link_type`, plus `r0_keys_per_second`, `alpha`,
`distance_km` and `pqc_capacity_keys_per_s` when its `node.yml` declares
them, plus the measured rate once its estimator has one:
`measured_rate_keys_per_s`, `measured_quality`, `measured_age_ms`). An ORR
announces its gRPC address and the QKC it hangs
off; a DKMS announces its SAE address (`host`), its ORR, its ETSI-020
`peer_addr` and the SAEs it serves. Everything derived (adjacency, edges,
ORR-by-QKC and DKMS-by-QKC indexes, SAE bindings) is rebuilt from that.

Four properties hold the design together — **announcements are
idempotent**, **a missing anchor is "not yet", not an error**, **conflicting
link metadata keeps the existing value**, **a QKC is authoritative over the
links it declares** (`Topology::declared`). Each one was the fix of a
measured failure, described in [Topology is inferred, never
configured](../docs/engineering-notes.md#topology-is-inferred-never-configured),
and they are walked through with the lifecycle they produce in
[auto-configuration.md](../docs/auto-configuration.md#3-what-the-sdn-derives).
What they look like from inside this module: every version bump re-pushes
the forwarding tables to every QKC and broadcasts a `TopologyEvent`, which
is why `presence` and the measured-rate reports live outside the `Topology`
snapshot (a timestamp or a noisy value inside it would make every heartbeat
look like a change); a missing anchor answers `accepted: false` with
`waiting_for` (a QKC's missing neighbour is listed in `edges_pending`); a
metadata conflict logs `link metadata disagrees between endpoints`.

Two more rules from the same note: one ORR and one DKMS per QKC (a second
one is rejected with `accepted: false` and a `reason`, see [One ORR and one
DKMS per QKC](../docs/engineering-notes.md#one-orr-and-one-dkms-per-qkc)),
and a SAE already bound to another DKMS is not re-bound by an announcement
(`sae_conflicts` in the response; the owner releases it with `PUT`/`DELETE
/sae/<id>` or by expiring).

### What an announcement gets back

The status fields and the peer set of each reply (`QkcAnnounceOutcome` and
its ORR/DKMS counterparts in `src/topology.rs`) are tabulated in
[auto-configuration.md § What comes back](../docs/auto-configuration.md#4-what-comes-back).
Peer sets are always sorted by id, and a neighbour that never announced a
`peer_addr` is omitted with a warning rather than handed out unreachable. The
module treats its own `node.yml` as a floor: the SDN may add peers and remove
the ones it added, never a locally declared one. See [Peers ride back on the
announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement).

### Presence and eviction

`presence` records the last announcement per (kind, id). A sweeper runs every
`presence_ttl_secs / 3` and deletes entities silent for more than the TTL;
`delete_qkc` cascades to the QKC's edges and forgets its measured-rate
reports, so a restarted QKC cannot pin an edge with a stale measurement.
Each entity expires on its own, without cascading to its ORR or DKMS. The same
sweep evicts demand reports older than `demand_ttl_secs` (default: the
presence TTL; `0` disables), so a DKMS that stopped reporting does not leave a
ghost commodity in the allocator. `presence_ttl_secs = 0` disables the
sweeper, and with it both expiries.

### Capacity model

`EdgeMeta::capacity_keys_per_second` is the single decision point, used by
routing and by every allocator:

| Link type | Capacity (keys/s) | Source |
|-----------|-------------------|--------|
| `qkd`, no measurement | `r0 · 10^(−alpha·distance_km/10)` | `r0`/`alpha`/`distance_km` from the QKC's `node.yml` (announced as `r0_keys_per_second`/`alpha`/`distance_km`; defaults 2000, 0.2, 0) |
| `qkd`, measured | the in-situ rate, min of both endpoints | `measured_rate_keys_per_s` in the announce, kept in `TopologyStore::measured` |
| `pqc` | `capacity_keys_per_s` as declared (announced as `pqc_capacity_keys_per_s`) | default 10 000 |

Note that the capacity is not R0: see [Theoretical rate — NOT
R0](../docs/engineering-notes.md#theoretical-rate--not-r0). The measured
value only crosses into the snapshot when it drifts from the current
capacity by at least 5 % of it (and at least 0.5 keys/s), the same
hysteresis `POST /link-capacity` uses; noise never bumps the version, two
endpoints measuring differently is not a conflict, and a heartbeat without a
measurement does not erase the applied value. Reports of quality
`unavailable` do not vote in the minimum. Design and rationale in [QKD rate
estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01).

### Routing, decoupled from rates

Forwarding tables come from `mcmcf::wcmp_from_topology`: for every
destination a BFS by hops, and each QKC splits across its neighbours that are
strictly closer to the destination, so the tables are loop-free by
construction even though each QKC decides on its own. The weight of a next
hop is the bottleneck capacity of the best downhill path through it. A
separate QKD-only table (`wcmp_qkd`) never uses `pqc` edges. Tables are
deterministic: the same snapshot yields the same bytes.

A watcher ticks every 200 ms; when `topology.version` or the published
snapshot changes it POSTs `{"replace": table}` to every QKC's admin endpoint
(`host` from the announcement, `http` or `https` depending on `[tls]`) and,
on a version change, broadcasts a `TopologyEvent` to gRPC subscribers. If any
QKC fails, nothing is marked as pushed and the whole push is retried on the
next tick. The QKD-only table is pushed only with `SDN_DUAL_GRADE_TABLES=1`.

Routing changes at topology cadence and never from buffer levels. The reason
is in [Active known issues](../docs/engineering-notes.md#active-known-issues--gotchas)
("Routing is decoupled from the rate solver").

### Rates

A commodity is an ordered pair `(src_dkms, dst_dkms)` with a valid QKC anchor
on both sides and on different QKCs, per key grade (`qkd` if a strictly-QKD
path joins the two QKCs, `pqc` otherwise). Each DKMS reports, per peer, its
buffer `level`, `capacity`, the EWMA `drain_rate` of SAE consumption and the
`grade` (`POST /demand`, every `demand_refresh_ms`, 1000 ms by default).
Pairs without a report get a synthetic `(0, 4096, 0)` entry in their natural
grade so the allocator has work at boot.

| `rate_allocator` | Method | Use |
|------------------|--------|-----|
| `num` (default) | dual-decomposition prices over the fixed WCMP fractions, `x_k = (w_k / Σ f·μ)^(1/α)` with `num_alpha = 1` (proportional fairness), `num_gamma = 0.2`, `num_fill_weight = 0.1` | production; a commodity with demand cannot starve |
| `maxmin` | exact progressive-filling waterfill, drain first, fill on the residual | exact lexicographic max-min |
| `lp` | the MCMCF-λ linear program (`src/mcmcf.rs`, backend `SDN_SOLVER`, default `highs`) | oracle and reference only |

All three produce the same `McfSnapshot`, served on `GET /rate/<dkms_id>` as
per-peer `enc`/`dec` rates with `qkd_available`, `reachable` and per-grade
values; each DKMS polls it every `rate_refresh_ms` (1000 ms by default). The
`num` and `maxmin` allocators run at `mcf_period_ms`; only `lp` goes through
the debouncer (`push_debounce_ms`, default 100). Why the LP is no longer the
production path is in [Solver &
fairness](../docs/engineering-notes.md#solver--fairness); the fairness
numbers behind that decision are in [Roadmap and measured
state](../docs/engineering-notes.md#roadmap-and-measured-state) and in the
[campaign results](../docs/results/campaign-2026-09.md).

### Control-plane security

With `[tls]` the HTTP admin and the gRPC server present `cert_path`/`key_path`
and require a client certificate signed by `client_ca` (the network CA). The
mutating routes bind the body to the certificate's SAN: a QKC with id `3`
must present `qkc-3`; an ORR and a DKMS must present the certificate named
exactly like their id (`orr_3`, `dkms-3` under the `gen-certs.sh`
convention); `POST /demand` accepts only the reporting DKMS's own id; `POST
/sae` and `/sae-bulk` only bindings whose `dkms_id` is the caller; `PUT`/
`DELETE /sae/<id>` only the DKMS that currently owns the SAE; `POST
/link-capacity` only the `sdn` identity or one of the two endpoint QKCs. A
mismatch is logged as `identity mismatch, rejecting` (`link-capacity no
autorizada` for the last one) and answered 403. Read-only routes need a
client cert too on `http_addr`; an optional plaintext mirror of them can be
opened on `http_ro_addr`. The forwarding push then goes out over https with
the SDN's own certificate, which is the only identity the QKC accepts a table
from. Without `[tls]` everything is plaintext and anyone with network access
can register nodes or rebind SAEs: acceptable only on a trusted internal
network. Model and phases in
[SECURITY.md](../docs/SECURITY.md#fase-3--plano-sdn-tls-en-todo--registro-con-identidad).

### The gRPC surface

`SdnControl` (`proto/sdn.proto`) is deliberately small: `GetSaeBinding`
(the DKMS), `GetOrrPath` (the ORR's onion modes) and `StreamTopology`
(cache invalidation on a version bump, at most 256 subscribers, a lagging
one is dropped) are the RPCs anything calls; `PutTopology` and `UpdateLink`
answer `UNIMPLEMENTED` because the graph is built from announcements and a
whole-graph push would be a second source of truth that cannot expire
nodes. The full table, with the state and caller of every RPC, is in
[ipc.md § SdnControl](../docs/ipc.md#sdncontrol--served-by-the-sdn-on-19000-sdnproto).

### When something restarts

A restarted SDN boots empty and is repopulated by the next heartbeat of every
module (`sdn_announce_secs`, 30 s by default; a module whose announcement is
not accepted yet, or a QKC with pending edges, retries from 2 s with
exponential backoff up to that period). Modules keep their `node.yml` links
and peers throughout; the ones the SDN had added are dropped by the first
accepted heartbeat against the empty SDN and come back as the neighbours
re-register. A restarted module keeps its id, so its announcement is an
update, not a new entity; if it was gone longer than the TTL it re-enters as
new and its peers learn it on their next heartbeat. The end-to-end flows this
participates in are in [architecture.md](../docs/architecture.md); the
convergence sequence in [auto-configuration.md](../docs/auto-configuration.md).

## Deployment

### Minimal node.yml

The SDN's `node.yml` carries no topology. `docker/examples/node.sdn.yml`
is almost empty; the fields the renderer (`docker/render_config.py`,
`render_sdn`) understands:

```yaml
# listen_ip: "0.0.0.0"       # optional
# ports:                     # optional; override only on port clashes
#   grpc: 19000
#   http: 19002
#   metrics: 19010
# presence_ttl_secs: 90      # optional; 3 missed announcements. 0 disables expiry
# mcf_period_ms: 5000        # optional; rate recompute cadence
# push_debounce_ms: 100      # optional; LP debouncer only
# control_tls: true          # optional; default true -> [tls] with sdn.crt/sdn.key
# certs_dir: /config/certs   # optional; where the certs are mounted
# cert_name: sdn             # optional; name of the cert files
# http_ro_port: 19003        # optional; plaintext read-only mirror on 127.0.0.1 (http_ro_bind: to change)
# extra:                     # optional; any other src/config.rs key, e.g.
#   rate_allocator: num      #   rate_allocator, num_alpha, demand_ttl_secs
```

The renderer always writes `node_id = "sdn"` and `presence_ttl_secs`; the
other defaults come from the binary (`src/config.rs`). The compose file
`docker/compose/sdn.yml` runs with `network_mode: host` and mounts
`./node.yml` and `./certs` under `/config`.

### Ports

| Port | Protocol | Who connects | TLS |
|------|----------|--------------|-----|
| 19000 | gRPC `SdnControl` | every DKMS (the ORR's optional channel does not come up under `control_tls`, see [ipc.md](../docs/ipc.md#tls-on-the-grpc-planes)) | mTLS with `[tls]`, plaintext otherwise |
| 19002 | HTTP admin: `/register/*`, `/demand`, `/rate`, `/sae*`, `/link-capacity`, read-only views | every QKC, ORR and DKMS; the operator | mTLS with `[tls]`, plaintext otherwise |
| 19003 (example) | plaintext read-only views | operator, `curl` | none; only if `http_ro_port` is set, bound to 127.0.0.1 unless `http_ro_bind` says otherwise |
| 19010 | HTTP `/metrics` (Prometheus) | monitoring | none |

The SDN itself dials every QKC's admin port (20002) to push forwarding tables.
Firewall rules in
[docker/README.md](../docker/README.md#ports-who-connects-to-whom).

### Certificates

| File | Role |
|------|------|
| `net-ca.crt` | the federation's network CA: verifies every client certificate on 19000/19002 (`client_ca`) |
| `sdn.crt` / `sdn.key` | the SDN's node certificate signed by `net-ca` (`gen-certs.sh sdn <ip>`); server identity on both planes and client identity when pushing forwarding tables |

Every module announcing over https needs its own `net-ca` certificate named
by the same convention (`qkc-<id>`, `orr_<n>`, `dkms-<n>`), because that
name is what the identity binding compares against. All leaves must hang off
one and the same `net-ca`. Keys are ML-DSA-65 and TLS negotiates only the
hybrid `X25519MLKEM768`; the binary self-checks a handshake with its own
identity at boot and aborts otherwise. Details in
[Security and firewall](../docker/README.md#security-and-firewall).

### Running without containers

```bash
./scripts/run-sdn.sh                       # cargo run --release -p sdn
CONFIG_DIR=/path/to/dir ./scripts/run-sdn.sh
```

Configuration is `sdn/config/default.toml` ← `local.toml` ← environment
with prefix `SDN__` (nested keys with `__`, e.g. `SDN__presence_ttl_secs=120`,
`SDN__tls__cert_path=...`). `default.toml` binds 19000/19002/19010 on
`0.0.0.0` with no `[tls]`, i.e. plaintext. `SDN_RATE_ALLOCATOR` and
`SDN_SOLVER` override the allocator and the LP backend. Never set
`SDN_DISABLE_LEX_REFINEMENT=1` (see the engineering notes).

Full procedure: [docker/README.md, section 1](../docker/README.md#1-sdn-central-operator)
and the [quick start](../docker/examples/quick_start.md).

## Health and diagnostics

Every 5 s the SDN logs `topology.state`:

| Field | Meaning |
|-------|---------|
| `version` | snapshot version; bumps only on real change |
| `qkcs`, `orrs`, `dkms`, `saes`, `edges` | entity counts |
| `declared` | QKCs declaring at least one link; `declared > 0` with `edges = 0` means the declared neighbours have not registered yet |
| `announced` | entities with a live heartbeat. Presence is only touched when an announcement is accepted, so this equals `qkcs + orrs + dkms`; a module that announces and is not accepted (missing anchor, or a second ORR/DKMS on a QKC) appears in neither, only in its own `anunciado a la SDN accepted=false` line |

The other periodic lines:

| Line | Key fields | Healthy |
|------|------------|---------|
| `forwarding push done` | `topo_from`, `topo_to`, `qkcs_ok`, `qkcs_err`, `snap_changed` | `qkcs_err = 0`; transient errors while QKCs boot are normal |
| `MCMCF-λ recomputed` | `allocator`, `n_commodities`, `n_edges`, `lambda`, `flows_with_rate`, `registry_len`, `elapsed_ms` | `n_commodities` = one per ordered DKMS pair on different QKCs (N·(N−1) for N DKMSs on N QKCs); `registry_len` grows as DKMSs report |
| `mcmcf.num` (every 5th recompute) | `served_total`, `with_demand`, `starved`, `priced_edges`, `max_price` | `starved = 0` whenever a route exists |
| `mcmcf.maxmin` | `served_total`, `min_drain_frac` | `min_drain_frac` is the worst served fraction |
| `mcmcf.solve` (`lp` only) | `lambda`, `t_floor`, `sigma_total`, `rates_zero`, `eta_fallback`, `dup_pairs`, `backend` | `eta_fallback = false`, `dup_pairs = 0` |
| `qkc registered` | `added`, `removed`, `pending` | logged on change only |
| `orr registered`, `dkms registered` | the id and its anchor | logged on change only |

Prometheus on 19010: `sdn_demand_post_total{outcome}`,
`sdn_demand_post_duration_seconds`, `sdn_lp_solve_duration_seconds`.

Read-only views for the operator (with `[tls]`: use the client certificate,
or `http_ro_port`): `GET /healthz`, `/topology` (counts and version),
`/qkcs`, `/orrs`, `/dkms`, `/saes`, `/links` (per edge: parameters,
`capacity_keys_per_second`, `measured_keys_per_s` and the raw
`measured_reports` per endpoint), `/demand` (the registry),
`/rate/<dkms_id>`, `/wcmp` (the published tables),
`/sae/<sae_id>/binding`, `/sae-bindings/<dkms_id>`, `POST /paths`.

Symptoms an operator will meet:

- **`qkcs_err > 0` that never clears** in `forwarding push done`, with
  `forwarding push failed` naming the QKC. The SDN cannot reach that QKC's
  admin port: `advertise_ip` wrong, 20002 filtered from the SDN, or one side
  has `[tls]` and the other does not (the scheme follows `control_tls` on
  both). See [Security and firewall](../docker/README.md#security-and-firewall).
- **A module announces but never appears.** Its own log shows `anunciado a
  la SDN accepted=false waiting_for=<anchor>`: the QKC of an ORR or the ORR
  of a DKMS is unknown, or that anchor's QKC already has an ORR/DKMS — the
  SDN then answers a `reason` and logs `two ORRs on the same QKC` (or
  `DKMS`) once, then `anchor conflict, still rejecting` at debug. Under mTLS,
  `identity mismatch, rejecting` on the SDN means the certificate name does
  not match the announced id. See [One ORR and one DKMS per
  QKC](../docs/engineering-notes.md#one-orr-and-one-dkms-per-qkc).
- **`link metadata disagrees between endpoints`.** The two QKCs declared
  different `r0`/`alpha`/`distance_km`/`link_type` for the same link; the
  existing value is kept until both `node.yml` agree. See
  [Topology is inferred](../docs/engineering-notes.md#topology-is-inferred-never-configured).
- **`módulo caducado` for a module that is running.** Its announcements are
  not arriving: check its `sdn_url` (`sdn_endpoint` on a DKMS), the 19002
  path, and that `presence_ttl_secs` exceeds its `sdn_announce_secs`
  (default 90 vs 30).

## Where things live

| File | Responsibility |
|------|----------------|
| `src/main.rs` | boots the gRPC server, the HTTP admin and the background tasks |
| `src/config.rs` | `SdnConfig`, `SdnTlsCfg`, defaults |
| `src/service.rs` | `SdnService`: presence sweeper, `topology.state`, forwarding push loop, rate recompute pipeline |
| `src/topology.rs` | the graph, announcements, `declared` links, `EdgeMeta` and its capacity, `MeasuredRates`, peer sets |
| `src/presence.rs` | last-heard registry, kept outside the snapshot |
| `src/http_api.rs` | admin routes, input validation and identity binding |
| `src/mtls.rs` | mTLS listener that injects `PeerCertIdentity` |
| `src/grpc_server.rs` | `SdnControl` implementation |
| `src/demand.rs` | `DemandRegistry`, one report per commodity |
| `src/mcmcf.rs` | `wcmp_from_topology` (routing) and the MCMCF-λ LP |
| `src/rates_num.rs` | the `num` and `maxmin` allocators |
| `src/mcf.rs` | `McfSnapshot` and the wire types the push and `/rate` serialise |
| `src/push.rs` | `StreamTopology` subscribers |

`debounce.rs`, `routing.rs`, `link_admission.rs`, `metrics.rs` and
`error.rs` are the rest. The rustdoc has the details: `make doc-open`.

## Further reading

- [Architecture](../docs/architecture.md) and
  [auto-configuration](../docs/auto-configuration.md): the end-to-end flows
  and how a federation converges.
- [IPC](../docs/ipc.md): gRPC and the protobuf schema (`proto/sdn.proto`).
- [Deployment guide](../docs/deployment.md) and
  [docker/README.md, section 1](../docker/README.md#1-sdn-central-operator);
  [adding a new institution](../docker/README.md#adding-a-new-institution).
- Engineering notes: [Topology is inferred, never
  configured](../docs/engineering-notes.md#topology-is-inferred-never-configured),
  [Theoretical rate — NOT R0](../docs/engineering-notes.md#theoretical-rate--not-r0),
  [Defaults](../docs/engineering-notes.md#defaults),
  [Scope boundaries](../docs/engineering-notes.md#scope-boundaries-do-not-implement),
  [Solver & fairness](../docs/engineering-notes.md#solver--fairness),
  [QKD rate estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01).
- [SECURITY.md](../docs/SECURITY.md): trust model and the control-plane phase.
- [Campaign 2026-09](../docs/results/campaign-2026-09.md): fairness across
  60 cells (6 topologies × N = 10..100), where the `num` allocator's per-pair
  spread is measured; and the [local mesh](../tests/local-mesh/README.md) and
  [testbed](../tests/testbed/README.md) harnesses.
