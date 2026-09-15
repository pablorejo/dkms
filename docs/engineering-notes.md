# Engineering notes

Design invariants, defaults and the gotchas measured along the way. This is
the project's technical memory: read it before touching the topology, the
rate path or the key-material path. Dates mark when something was measured
or decided.

## What this repo is

Rust rewrite of [`dkms`](https://github.com/pabloprejo/dkms). DKMS digital-twin
for QKD + PQC workflows. Five runtime modules (QKC, ORR, SDN, DKMS, quditto)
each as a separate binary crate plus `common/`. Workspace `Cargo.toml`.
Protobuf in `/proto/` compiled by `common/build.rs` into `common::proto::*`.

This repository targets **real multi-host deployment**: one Docker image per
module, each institution runs `docker compose up` filling in a short
`node.yml`. No central orchestrator. See `docker/README.md`.

The Python orchestator + web UI + k8s/EKS simulation harness used to live here
too; they were removed on 2026-07-30 and now live on the `tests_aws` branch
(campaigns on EKS `dkms1`, eu-north-1) and `tests_cesga` (SLURM).

## IPC

- **gRPC (tonic)** for the control plane (DKMS↔SDN, DKMS↔ORR, etc.). Schemas in
  `/proto/`. Prefer gRPC for any new RPC.
- **Binary TCP** only for the QKC↔QKC hot path. Wire format in `docs/ipc.md` /
  `wire/src/lib.rs` (`common::ipc::binary_tcp` is a re-export shim of the
  `wire` crate; the file of that name does not exist). Don't add a third transport.

## Common patterns

- **Logging**: `tracing` macros + `common::logging::init()` (driven by `RUST_LOG`).
- **Config**: per-crate `Config` in `src/config.rs`. Load via
  `common::config::load::<C>("MODULE")`: `config/default.toml` ← `local.toml` ←
  env (prefix `MODULE__`, nested with `__`, e.g.
  `DKMS__buffer__capacity_per_peer`). An env override of ONE nested field
  **merges** into the file's section (the other fields survive) — pinned by
  `common::config::tests::an_env_override_of_one_nested_field_merges_into_the_section`
  (2026-08-30). This file used to claim the opposite ("replaces the whole
  section"); that was never true with config-rs 0.14.
- **Errors**: per-crate `Error` enum with `thiserror`. Cross crate boundary as
  `anyhow::Error` only in `main.rs`.
- **IDs**: use `common::ids::{NodeId, SaeId, KeyId}` newtypes, not bare `String`.
- **Metrics**: Prometheus via `common::metrics`. Default `:9100`. (Rust DKMS
  `/metrics` is empty — gauges/counters never registered; read DKMS state from
  the `generator.state` log line emitted every 5 s.)

## Build / run

```bash
cargo build --release [-p <crate>]
cargo test  --workspace
cargo clippy --workspace -- -D warnings
cargo fmt   --all
make doc          # rustdoc, warnings are errors, private items included; make doc-open to read it
```

**rustdoc is part of `make check`** (since 2026-09-15): a broken intra-doc
link, a `<T>` outside backticks or a link from public docs to a private item
fails the gate. Private items are documented on purpose — four of the five
crates are binaries, so their public API is not the interesting part. The
`docs` workflow publishes the same tree to GitHub Pages from `main` (public
repo only; Pages must be enabled once with Source = "GitHub Actions");
`scripts/rustdoc-index.py` writes the landing page from `cargo metadata`, so
the crate descriptions in each `Cargo.toml` are what a reader sees first.

**`make check` en local NO es el de CI si no tienes rustup.** El
`rust-toolchain.toml` pina **1.88** (lo mismo que carga CESGA con
`module load rust/1.88.0`), pero sin rustup ese pin se ignora en silencio y
corre el rust del sistema — en una máquina con rust 1.98, cuyo clippy ya no marca
lints que 1.88 sí (mordió el 2026-08-31: `uninlined_format_args` en
`tests/loadgen`, CI rojo con el `make check` local en verde). Para validar de
verdad antes de empujar:

```bash
docker run --rm -e DKMS_NO_TEST_SKIPS=1 -v "$PWD":/w -w /w rust:1.88-trixie \
  bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler cmake \
           clang libclang-dev make openssl python3 python3-yaml && make check'
```

Per-module run scripts in `scripts/run-*.sh`. Local multi-module demos in
`scripts/demo-3qkc/` and `scripts/demo-star/`. Container images via
`make images` (or `docker buildx bake -f docker/docker-bake.hcl`).

**Only `docker/Dockerfile` produces deployable images.** It carries
`entrypoint.sh` + `render_config.py`, which turn the mounted `node.yml` into
the binary's native TOML. `docker/Dockerfile.workspace` runs the bare binary,
so a compose deployment built from it crash-loops — the QKC dies immediately
with `required arguments were not provided: --config`, the others start
without their config. `make images` and `scripts/deploy-images.sh` were
pointing at the workspace file and now point at `docker/Dockerfile`
(2026-08-02); if a redeploy ever crash-loops right after a build, check this
first.

## Deployment

- **Multi-host, per institution** — `docker/README.md` (the only maintained
  path). Each image renders `/config/node.yml` into the binary's native TOML
  at boot. The old `<module>/k8s/` manifests predated this contract (no
  config mount, pre-2026 ports) and were removed 2026-08-31; if k8s is ever
  needed again, write manifests that mount `node.yml` + certs under
  `/config` and use the current ports.

The SDN needs no topology file — see "Topology is inferred, never
configured" below. gRPC clients dial lazily, so partial deployments degrade
to retries rather than failing.

## Saturation / load tests — run isolated

Local saturation tests have OOM-killed a desktop session (Brave, pipewire, dbus,
systemd manager evicted). **Always run inside a memory-bounded cgroup**:

- `systemd-run --user --scope -p MemoryMax=8G -p MemorySwapMax=4G -- <cmd>`, or
- a reusable user slice (`MemoryMax=8G`) via `--slice=stress`, or
- Docker / k8s with explicit `--memory` / `resources.limits.memory`.

On a 16 GB box cap tests at ~10–11 GB. Watch with `systemd-cgtop` / `btop`;
investigate if RSS > 70 % of cap. `systemd-oomd` + `zram` reduce blast radius
but don't replace cgroup caps.

Test artifacts go under `tests/results/<campaign>/`, which is gitignored —
never commit them. **No artifacts in `/tmp`**: it clears on reboot.

## Theoretical rate — NOT R0

Capacity for the MCF solver is **not** R0. It's:

```
cap_edge = R0 × 10^(-α·d/10)
```

(`sdn/src/topology.rs :: quditto_capacity_keys_per_second`). With α=0.2,
d=5 km the factor is 0.794, so an R0=2000 edge has cap 1588.7 kps. Fair share
with N flows is `cap_edge / N`, never `R0 / N`. Always compute this before
claiming a "deficit vs theoretical".

**Since 2026-09-01 the QKD rate is also MEASURED in situ** by the QKC
(`qkc/src/rate_estimator.rs`) and announced to the SDN each heartbeat; a
measured value that drifts ≥5 % from the current capacity overrides the
formula (`EdgeMeta::measured_keys_per_s`, min of both endpoints). The
formula/`r0` is then just the cold-start prior — see the estimator gotcha
below. On quditto the prior and the measurement agree, so the override
usually stays dormant in sims; on real hardware it is the whole point.

## Defaults

- **R0 = 2000 keys/s**. Raising it to 10000 does not increase what the SDN can
  allocate.
- **buffer_enc_size = 4096** default. The CLI flag `--buffer-enc-size` only
  affects `--sat-threshold`.
- **N viable ≈ 20** for the `lp` allocator with `microlp`. N=30 marginal
  (cyclic OOMs), N≥31 unviable (LP memory scales O(N²)). The default `num`
  allocator has no LP: its cost is O(edges + commodities·path) per tick.
- **PQC link capacity = 10 000 keys/s** unless declared
  (`links[].capacity_keys_per_s`).
- **rate_allocator = num** (α=1, γ=0.2, fill_weight=0.1); `SDN_SOLVER = highs`
  for the `lp` mode.

## Things to NOT do

- No third transport — gRPC or binary TCP.
- No business logic in `main.rs` — keep it in `service.rs`.
- No `pub use` re-exports of internal types across crate boundaries. The
  boundary between modules is the protobuf schema.
- No HTTP endpoints on QKC/ORR/quditto — only DKMS and SDN expose HTTP.

## Topology is inferred, never configured

There is no topology file. The SDN boots with an empty graph and builds it from
what the modules announce over its HTTP admin:

- `POST /register/qkc` — a QKC declares itself and its links (with
  `r0`/`alpha`/`distance_km`, which it does not use itself: they are for the
  SDN's capacity model).
- `POST /register/orr` — an ORR declares which QKC it hangs off.
- `POST /register/dkms` — a DKMS declares its ORR and the SAEs it serves.

Four properties hold the design together; break any of them and it thrashes:

1. **Announcements are idempotent.** The announce loop doubles as a heartbeat,
   so an unchanged announcement must not bump `topology.version` — every bump
   re-pushes forwarding tables and re-runs the LP.
2. **A missing anchor is "not yet", not an error.** An edge needs both QKCs
   registered, `upsert_orr` needs its QKC, `upsert_dkms` needs its ORR. The
   announcers retry with backoff, so any boot order converges.
3. **Conflicting link metadata keeps the existing value** and logs. Last-write
   -wins would make two disagreeing endpoints overwrite each other forever.
   A *single* declarer is not a conflict — there is nobody to disagree with,
   so its changes apply.
4. **A QKC's announcement is authoritative over the links it declares.**
   `Topology::declared` remembers what each QKC said, and the edge (a,b)
   exists iff `a` declares `b` or `b` declares `a`. One end is enough, on
   purpose in both directions: a QKC booting with nothing declared cannot
   erase what its neighbour declares, and retiring your own link does not
   depend on the neighbour retiring it too. Without this the announcement was
   purely additive — an edge only died when a whole node expired, so a link
   could not be rewired nor its physical model corrected, and the SDN handed
   the QKC back the link the operator had just removed (measured 2026-08-20).

`crate::presence` tracks the last announcement per entity — **outside** the
`Topology` snapshot, since that is compared wholesale for change detection.
A module silent for `presence_ttl_secs` (default 90) is dropped along with its
edges.

### Peers ride back on the announcement

The graph is only half of it: the modules also need to know *who their peers
are*, or a new node is visible to the SDN and invisible to everyone else (the
QKC drops its neighbour's handshake with "unknown neighbor" and no key flows).
So the announce **response** carries the peer set, derived from the graph by
`Topology::{qkc_peers,orr_peers,dkms_peers}`. No new channel and no extra
polling loop — it converges because a node registers and the next heartbeat of
its peers carries the update. Adding an institution touches no running node.

Four things to keep in mind when touching this path:

- **`node.yml` is a floor, not a snapshot.** The SDN may add peers and remove
  the ones it added, but a locally declared peer stays. Without that rule, every
  boot race and every moment the SDN is behind destroys live links and live key
  material — observed in the lab, a QKC tearing down both of its own configured
  links seconds after boot and rebuilding them 30 s later.
- **Peer sets must be sorted by id, always.** Each module diffs what it receives
  against what it holds to decide adds and drops; an unstable order reads as a
  change on every heartbeat and thrashes.
- **What a module announces is not what its peers dial.** A QKC announces its
  admin HTTP (20002, where the SDN pushes forwarding) but neighbours connect on
  20000; a DKMS announces its SAE port (20005) but peers reach it on 20006.
  Hence `peer_addr` on `Qkc`/`Dkms`. A neighbour without one is omitted from the
  peer set with a warning rather than handed out broken.
- **Only PQC links are created from the SDN.** A QKD link needs a `kme_url`
  pointing at that institution's own KME, which the SDN cannot know or invent,
  so those stay in `node.yml`; the announcer warns when the SDN offers a QKD
  link it has no local config for.

QKC `links` and `ForwardingTable.direct`/`direct_qkd` are `ArcSwap` so links can
be built and torn down at runtime (`add_link`/`remove_link`). Registering the
neighbour in the routing table is not optional — without it the link exists and
is never routed over, which looks exactly like the bug this replaced. Tearing a
link down drops its `SecretStore`, whose epochs are `Zeroizing`, so the material
is wiped; frames in flight hold their own `Arc` and finish.

### One ORR and one DKMS per QKC

`orr_by_qkc` / `dkms_by_qkc` are 1:1 and keep the **last** announcement, so a
QKC that ends up with two ORRs silently loses one: the displaced ORR stops
being resolvable by its QKC and the material goes out encrypted for the wrong
one. Measured 2026-08-20 by re-anchoring an ORR onto an occupied QKC —
`recv_corrupt` at 100 % on everything one DKMS sent, while everything it
received was fine and the topology looked perfect. `upsert_orr`/`upsert_dkms`
now log it; the 1:1 model itself has not changed.

Re-anchoring is otherwise supported: an announcement that moves an ORR to
another QKC (or a DKMS to another ORR) clears the index entry of the one it
left. Before 2026-08-20 only the manual `update_orr`/`update_dkms` did that,
so an anchor change over the announce path left the old QKC pointing at a
module that had moved — and that index is what `GetOrrPath` builds the
ORR-level path from.

## Scope boundaries (do not implement)

- **`PutTopology` / `UpdateLink`** still return `UNIMPLEMENTED`. Topology is
  mutated by registration, not by pushing a whole graph.
- **Per-direction QKD rates** — `quditto.R0` is shared per link; both endpoints
  compete for the same `fresh` FIFO. Don't model A→B and B→A separately.

## Active known issues / gotchas

- **The generator emits to every peer at once, bounded by `max_emits_in_flight`
  (default 128).** It used to walk its peers one at a time, awaiting each
  batch, so a round took the sum of the latencies instead of `tick_ms` and the
  per-peer rate fell as O(1/N) — worse exactly as the mesh grows. Measured on
  CESGA 2026-08-24 with nine peers: 2.6 rounds/s instead of ten, 83.7 keys/s
  per peer against the 139 the SDN allocated. Fixing it took the ring-QKD mesh
  from 7 594 to 13 666 keys/s (+80 %) and the per-pair rate from 84.4 to 151.8.
  **The bound is not optional**: unbounded, the pressure on the ORR multiplies
  by the peer count and this path has no back-pressure of its own — raising the
  token ceiling ×10 alone was enough to OOM the process at 16.6 GB. Failures
  stay aggregated per peer into one line per tick, or a peer whose ORR has no
  master secret yet produces 752 MB of logs in ten minutes.
- **Routing is decoupled from the rate solver — keep it that way.** WCMP
  tables come from `wcmp_from_topology` (topology + capacities: per
  destination, BFS by hops; split across **strictly closer** neighbours, so
  loop-free by construction; weight = bottleneck capacity of the best downhill
  path through that hop). The LP's `edge_flows` are **not** wired to the
  snapshot: routing with the LP's output coupled *where* to *how much*, and
  three independent paths ended with empty `edge_flows` — the buffers-full
  short-circuit (permanent, because the generator overshot `capacity_per_peer`
  by one batch: 4096 → 4107, `remaining()` clamped to 0 on all 90
  commodities), microlp's false phase-2 infeasibility at N≥20, and clarabel,
  which disables phase 2 by design. Net effect: the k-splittable multipath was
  never active in anything measured before 2026-08-24 — the QKC fell back to
  shortest path, some links ran dry (`enc=0 dec=0`, 355 k misses) while others
  idled. Even a perfect LP would flap routes every 5 s from instantaneous
  buffer levels; a route should change at topology cadence. The generator now
  also caps each batch to buffer headroom (`level ≤ capacity` is an invariant
  `/status` and the solver both assume). `wcmp_from_edge_flows` stays as an
  **unwired** demand-bias hook: if ever used, apply it *on top of* the
  topology base when the LP is healthy — never as the sole source.
  `mcmcf.diag` warns if `overfull > 0` reappears. Verified on the QKD ring:
  90 (src,dst) entries, 34 with ≥2 next hops, 90/90 byte-identical exchanges,
  and all three links of each transit QKC carrying traffic under load.
- **DKMS buffer/generator defaults are unit-test sized**: `capacity_per_peer
  = 4096`, `max_tokens_per_peer_per_tick = 32` (hard cap 320 kps). Remember the
  config-rs nested-merge caveat above when overriding them.
- **`enc_keys` buffers ARE on the data path** (verified 2026-06-29): the SAE
  session key (random, `OsRng`) is OTP-wrapped with a *transport key* popped
  from `buffer_enc[peer]` (`service.rs` `build_ext_keys_envelope` → `pop_oldest`)
  and delivered over HTTP/2 ETSI 020; the buffer is filled in background by the
  generator via ORR.
- **A restarted DKMS is refilled because its peers notice the restart.**
  `buffer_enc[peer]` here and `buffer_dec[me]` there are two copies of the same
  material, and the buffers are RAM-only, so a restart leaves both ends holding
  halves of keys the other no longer has. Nothing recovers on its own: the
  generator stops at `enc + ack_pending >= capacity_per_peer`, and what drains
  ENC is SAE demand. Measured 2026-08-20 — a restarted node emitted normally
  and sat at `dec=0 recv=0` against every peer indefinitely.
  Every `DKMS_BUFFER` now carries the sender's `incarnation`, a random id per
  process run (`southbound/orr.rs`). When a peer's changes, the receiver drops
  its `buffer_enc`, its `buffer_dec` and its `ack_pending` for that peer and
  the generator refills — measured at ~30 s end to end, no SAE request needed.
  Two things to keep in mind:
  - **The detector is the restarted node's own refill traffic**, not the ACKs.
    A DKMS that boots has an empty ENC so it emits immediately, whereas ACKs
    only arrive if *we* emit — and being parked at capacity is exactly the
    state to escape. A peer that comes back with its generator disabled is
    therefore not detected; the older reactive path (a `TransportKeyMissing`
    rejection on the first SAE request) still covers that.
  - **The first sighting of a peer is not a restart.** Treating it as one would
    throw away good material on every boot.
- **PQC edges carry a declared capacity since 2026-08-25** —
  `EdgeMeta::pqc_capacity_keys_per_s`, default 10 000, configurable per link
  (`links[].capacity_keys_per_s` in node.yml, announced by the QKC like
  r0/alpha/distance). This replaced the `1e9` sentinel that made the rate
  signal meaningless on PQC-only deployments (measured 2026-08-02: λ swinging
  2.7e5–2.1e6, DKMSs handed `sdn_rate = 8e8` alternating with 0.0). All
  capacity reads go through `EdgeMeta::capacity_keys_per_second()` — the
  quditto formula for QKD, the declared field for PQC; `POST /link-capacity`
  now works on PQC edges too (writes the field). Nobody measures this number:
  it is what the operator declares the link can sustain, and its default sits
  far above the per-pair token ceiling (320) and far below the old sentinel.
  Verified live 2026-08-25 on the 3-node mesh (`/links` shows 10 000; the
  handed rates are bounded and meaningful).
- **The LP backend default is `highs` since 2026-08-25** (`SDN_SOLVER`
  selects `highs|microlp|clarabel`; needs `cmake` to build — already in the
  three Dockerfiles and in `cesga.sbatch`). The reason is microlp: it reports
  a false `Infeasible` in the phase-2 LP in ~97-100 % of recomputes at
  N=20/380 commodities, and measured 2026-08-25 it was just as bad at
  N=10/90 under load (`eta_fallback=true` in 161/165 solves, CESGA job
  9266817) — the LP is trivially feasible at η=0; the bug is microlp's
  simplex, not the formulation. The `η = 0` fallback stays as a safety net
  for any backend. Note the LP is no longer the production rate path (see
  "Solver & fairness": `rate_allocator` defaults to `num`); the LP remains
  as oracle/reference via `SDN_RATE_ALLOCATOR=lp`.
- **NEVER set `SDN_DISABLE_LEX_REFINEMENT=1`**: disabling phase-2 freezes the
  SDN at the phase-1 rate `λ·R_k` and multiplies saturation time by ~10×
  (verified 2026-05-24: with the flag, 246/361 saturated in 600 s; without it,
  379/380). The microlp phase-2 bug is already handled by the η=0 fallback.
- **ORR bootstrap race on individual pod restart** — closed 2026-08-30. The
  passive re-bootstrap (`trigger_passive_rebootstrap`) has existed for a
  while but is only reachable from the onion data path, i.e. never with the
  default `max_hops = 0`. Now the rotation loop itself re-bootstraps a peer
  that answers «no bootstrap_secret» (`bootstrap::rebootstrap`, under the
  same in-flight guard), so a restarted ORR converges at the next rotation
  attempt (backoff ≤ 30 s), traffic or not.
- **A restarted QKC restarts its epoch counter at 1**, so the two ends of a
  link can end up with disjoint epoch windows, each encrypting with numbers the
  other never had. Every frame that arrives is then dropped undecrypted: the
  peer's DKMS shows `recv=0` and the sender's `expired` climbs forever, while
  `keystore.levels` still reports non-zero levels and the SDN topology looks
  perfect. Measured 2026-08-03 at 100 % loss on one link, permanent — it took a
  full redeploy to clear.
  **Fixed by reactive resynchronisation** (`pqc_source::SecretStore::
  request_resync` → `pqc_handshake::relink(peer_epoch)`): the DEC side reads
  the peer's epoch off the first 4 bytes of every `key_id`, so when it drops
  undecryptable keys it knows exactly which window the peer is on and asks for
  a relink; the initiator then negotiates a block **above both** windows.
  Two things to keep in mind:
  - **The relink base must clear the peer's window, not just ours.** Staying at
    our own `highest + 1` leaves the peer — which is higher — encrypting where
    we still can't follow, and it never converges.
  - **Recovery is reactive: a broken *idle* link stays broken** until something
    is sent over it, because the only evidence is an undecryptable frame. It
    heals within seconds of the first traffic. A test that waits for the link
    to fix itself while idle will measure the breakage, not the fix.
  Since 2026-08-30 the **responder can ask**: only the initiator may send
  INIT, but a responder whose DEC side sees epochs it lacks sends
  `FRAME_PQC_RESYNC_REQ` (0x28, `_AUTH` 0x29, `_SIGNED` 0x2A; payload =
  its highest epoch, same `pqc_auth` policy as INIT/RESP) and the
  initiator's rotation loop relinks **above both** windows
  (`handle_resync_request`). A one-directional split no longer needs a
  manual restart. Two invariants in that loop, both measured the hard way:
  the rekey timer lives **outside** the `select!` (rebuilding it per
  iteration reset the hour on every reconnect/resync — one rotation in 15 h,
  2026-08-24), and `establish()` is bounded (60 s) and reuses the pending
  keypair on retry — a fresh keypair for the same epoch would leave a RESP in
  flight that decapsulates to a different secret with no error.
- **The link MAC and the anti-replay window are checked in the connection
  READER, in arrival order** (`peer_server::handle_conn` → `frame_auth::
  authenticate`; `relay::handle_incoming` takes `pre_authenticated` and does
  not check again — `open` strips the trailer and restores the plain kind,
  so a second pass would reject the frame as plaintext under `require`).
  Until 2026-09-03 the check ran inside the dispatcher's concurrent tasks
  (up to `MAX_INFLIGHT_PEER` = 8192 in flight) against a 1024-counter window:
  under load a legitimate frame was checked after thousands of later ones
  and rejected as a replay. Measured on the CESGA campaign with `bad_mac = 0`
  everywhere: the star hub at N=100 rejected 542 598 of 72 M verified frames,
  the bridge endpoint at N=100 332 762 of 2.8 M (10 %) — expired keys and
  lost rate with nobody attacking. Keep the order invariant if the intake is
  ever restructured; the two `opened_*` tests in `frame_auth.rs` pin it. A
  side effect worth keeping: forged frames never reach the intake queue.
- **`tokio::select!` over a `JoinHandle` needs `biased`** — fixed 2026-08-02 in
  `orr/src/qkc_link.rs`, don't drop it. `run_session` selects over
  `send_rx.recv()` and `&mut read_task`. Without `biased`, `select!` picks
  randomly among *ready* branches, so with the reader already finished and a
  frame queued, the writer branch can win; the next iteration polls a completed
  `JoinHandle`, which tokio panics on ("JoinHandle polled after completion").
  With `panic = "abort"` in the release profile that kills the ORR — one node's
  ORR had racked up **10 restarts** before this was spotted, each one dragging
  its whole bootstrap with every peer along with it. The restarts are invisible
  in `docker ps` (which shows the container Up) and in `RestartCount`, which
  the compose restart policy leaves at 0 for `docker restart`; look at
  `docker inspect .RestartCount` on the *container* and at `panicked at` in the
  logs. `tests/testbed/t00_health.sh` now checks both.
- **Two concurrent `encap` on the same ORR pair diverge the secret** — fixed
  2026-08-02, don't reintroduce. The lex-smaller ORR is the sole initiator
  (`bootstrap.rs`), but *it* had two ways in: the announce-driven
  `bootstrap_peer` and `trigger_passive_rebootstrap`. `bootstrap_peer` guarded
  only with `if !peers.has_bootstrap(peer)`, a check-then-act, so both could
  run and each produced a different `shared_secret`; the peer kept the last one
  it received and the local side kept its own. Both now take
  `try_mark_rebootstrap_inflight`, **per attempt** rather than across the whole
  retry loop — hold it across the loop and the passive path, which is the one
  that refreshes a changed pubkey, can never heal a stuck handshake.
  Symptom if it regresses: one DKMS pair delivering 100 % corrupt transport
  keys in both directions (`recv_corrupt` climbing, `acked=0`, `expired`
  growing) while every other pair is fine. Reproduced by hot-adding a node.
- **PQC link recovery after a single-end restart is reconnect-driven** (fixed
  2026-07-31, both directions — don't re-derive it, and don't break the
  invariants below).
  Only the lex-smaller QKC — the initiator — may send INIT. A restarted
  responder cannot ask for a fresh epoch by sending one itself: the initiator
  would keep its old secret while the responder adopted the new one, and the
  link OTP has no MAC to catch the divergence. So the trigger is local. A TCP
  reconnect is the one thing that always happens when the peer restarts and
  never happens while it stays up, so `PeerOut` signals re-connections (the
  first connect is startup, not a reconnect) and the initiator's rotation task
  calls `relink()` (`pqc_handshake.rs`), which negotiates a fresh block of
  epochs **above** the current highest and prunes below it. Three invariants:
  - **Epoch numbers are never reused.** The same number carrying a different
    secret on each end is precisely what nothing downstream can detect.
  - **Prune after the handshake, never before**, or `enc_keys` is left with no
    live epoch while it runs.
  - **`establish()` has exactly one owner**, the rotation loop. Two concurrent
    calls for the same epoch race on `pending_sk` and the RESP gets decapsulated
    with the wrong sk — which is why `relink` is awaited inside that loop rather
    than spawned.

  `RELINK_MIN_INTERVAL` (5 s) keeps a flapping peer from rekeying on every
  bounce. A reconnect from a network blip rather than a restart also fires this;
  it costs three ML-KEM handshakes and breaks nothing.
  An idle link used to hide the reconnect entirely: the writer only learned the
  socket was dead when it next tried to write, and a peer restart that leaves
  the DKMS buffers full means no writes ever come. `writer_loop` now waits on
  the queue **or** `stream.readable()`; the peer channel is one-way (the peer
  writes over its own connection, see `peer_server`), so readable means EOF or
  error — `peer_hung_up` in `transport/peer_client.rs`. Unexpected inbound bytes
  are logged and ignored, not treated as a hangup.
  The mirror case — only the lex-**smaller** end restarting — is fixed
  separately: the responder re-encapsulates when the INIT carries a new pubkey
  instead of replaying its cached ciphertext (`handle_init`).
  Symptom if any of this regresses: `keystore.levels peer=N enc=0 dec=0 taken=0`
  plus `enc_refill failed … timeout waiting pqc-secret` every 10 s, while both
  ends log a healthy `handshake.established` history and the SDN sees the
  topology as fine.
- **On an OTP link, every byte added to the payload costs QKD key material, and
  the cost is stepped, not proportional.** `qkc/src/crypto.rs` chunks the
  payload into `key_size_bits / 8` blocks (32 B by default) and spends **one
  QKD key per block**, so 28 bytes of overhead turn a 32-byte message into two
  blocks — twice the material per hop. Measured on CESGA 2026-08-28 when the
  ORR onion's AEAD put `nonce ‖ ct ‖ tag` in the payload: **−51 % throughput**
  (5298 → 2582 keys/s) with `misses=369171` and `wenc_to=61551` where there had
  been zeros. It is invisible on PQC links, where key material is not the
  bottleneck, so **measure this kind of change on the QKD arm**.
  What to do instead: anything that isn't secret goes *outside* the encrypted
  payload. The onion's tag rides in the ORR header (cleartext, propagated
  byte-for-byte by the QKC, already covered hop-by-hop by the link MAC) and its
  nonce is derived from `key_id` rather than transmitted — which is also
  strictly safer, since the AEAD key is `HKDF(master_secret, key_id)`, so each
  key is used with exactly one nonce and GCM nonce reuse becomes impossible by
  construction. The link MAC's own 48-byte trailer is appended *after*
  `encrypt` and stripped before `decrypt`, so it never reaches the chunker.
- **DKMS↔ORR and ORR↔ORR gRPC run under mTLS BY DEFAULT since 2026-08-28**
  (`grpc_tls`, default `true`, in the ORR; `southbound.orr_tls`, default
  `true`, in the DKMS — it upgrades `orr_endpoint` from `http://` to
  `https://` at load). That plane carries the transport material, so plaintext
  is an explicit opt-out (`grpc_tls = false` AND `orr_tls = false`, both ends),
  acceptable only while DKMS and ORR share a host or a trusted internal
  network. An ORR with `grpc_tls` on and no `[tls]` refuses to start and says
  what it needs — never a silent plaintext fallback. The ORR presents its node
  cert (`gen-certs.sh <orr_id> <ip>`) and requires a network-CA client cert,
  so it covers both its DKMS and its ORR peers; peer URLs from the SDN or the
  TOML come as `http://` and `grpc_tls::peer_url` rewrites the scheme (the SDN
  neither knows nor should know about TLS), which makes the setting
  deployment-wide. Verified on the local mesh with ML-DSA-65 certs over tonic
  (X25519MLKEM768 negotiated): bootstrap with every peer, 12 288 messages,
  12/12 identical ETSI-014 exchanges. Knock-on defaults: `gen-certs.sh` emits
  `KEY_ALG=ml-dsa-65` (RSA is opt-in and not quantum-safe — SAE clients must
  verify ML-DSA, i.e. OpenSSL ≥ 3.5 or rustls), `mesh.sh` runs the mTLS arm
  (`DKMS_MESH_GRPC_TLS=0` is the comparison arm) and seeds certs from
  `certs-pregen/<alg>` where openssl cannot generate ML-DSA (CESGA has
  1.1.1g), `certs-pregen` must carry `orr_1..N`, `orr-test-client` reads
  `ORR_TLS_CERT/KEY/CA`, and the demo-star ORRs carry a `[tls]` block. Two
  traps:
  - **`grpc_tls` must be emitted BEFORE the `[tls]` table in the TOML.** A bare
    key after a table belongs to that table, so `tls.grpc_tls = true` parsed
    fine and the ORR started in plaintext with the option "set".
    `render_config.py` now orders it; keep it that way.
  - **`ls` here is aliased to eza with icons**, so `ls dir | grep -c "^orr_"`
    counts 0 even when the files exist. Use `/bin/ls` in scripts and checks.
  - **A pre-generated cert set must hang off ONE CA — check it, don't assume.**
    `gen-certs.sh` reuses whatever `net-ca` it finds, so two runs in two
    directories produce two different CAs with the same CN. Topping up a set
    with leaves signed elsewhere (measured 2026-08-28: `orr_N` emitted here,
    copied onto CESGA's own `certs-pregen`) makes every mTLS dial fail with
    `UnknownIssuer` — and tonic collapses that into a bare `transport error`,
    so the job just reported 0/90 buffers filled with no error anywhere.
    `mesh.sh` now compares each class's leaf AKI against its CA's SKI
    (dkms-1/net-ca, sae_1/sae-ca, orr_1/net-ca) and aborts; key ids work
    where `openssl verify` cannot, i.e. CESGA's 1.1.1g against ML-DSA. The
    DKMS and ORR now print the full cause chain (`{:#}`) on a failed dial.
- **TLS negotiates ONLY `X25519MLKEM768` since 2026-08-30** — no classical
  fallback in any plane (`common/src/tls_pqc.rs::build_provider`, the one
  provider `common::tls` uses for axum and tonic). Every binary self-checks a
  handshake with its own node identity at boot (`self_check_hybrid_kx`) and
  aborts if anything else gets negotiated; `install_process_default` is now
  `ensure_process_default` and aborts instead of running classical when
  another provider won the race. Consequence: SAE clients need OpenSSL ≥ 3.5
  or rustls — `tests/testbed/sae_load.py` refuses to start below 3.5 and the
  scripts prefer `tests/loadgen` (`target/release/sae_load`), same CLI and
  CSV. CESGA (1.1.1g) cannot run the Python client at all.
- **`DkmsControl` (gRPC :20007, `Drain` wipes every buffer, no auth) renders
  to `127.0.0.1` always** since 2026-08-30 regardless of `listen_ip`; only
  `control_addr` in `node.yml` opens it, and the DKMS warns at boot when it
  is not loopback. It used to render on `0.0.0.0` with `network_mode: host`.
- **Secrets never reach the log**: `link_psk` and `sign_secret_seed` are
  `common::config::SecretString` (`Debug` prints `<redacted>`, no `Display`).
  The modules log `info!(?cfg)` at boot and `docker logs` is the diagnostic
  channel — a derived `Debug` shipped every PSK in clear until 2026-08-30.
- **ORR announcements are bound to the node certificate since 2026-08-30.**
  `GetPublicKey` signs `(orr_id, suite, pubkey)` with the ML-DSA-65 key of
  `tls.key_path` and ships the chain (`signing_certs`); the peer verifies the
  chain against `control_plane_ca` and the SAN `dkms://<orr_id>`
  (`common::cert_identity`). `bootstrap_trust = strict` therefore needs no
  per-peer config and survives restarts (default still `tofu` until the
  testbed t30 passes). And `EstablishSecret` / the two rotation RPCs require
  the body's `from` to equal the mTLS client cert identity
  (`PERMISSION_DENIED` otherwise) — before, any net-CA cert holder could
  overwrite another ORR's `bootstrap_secret`. `sign_secret_seed` /
  `peer_verify_keys` are the legacy path.
- **A second ORR (or DKMS) anchoring to an occupied QKC is rejected** since
  2026-08-30 (`Upsert::Rejected` → `accepted: false` + `reason` in the
  announce response, so the announcer keeps retrying), instead of silently
  displacing the first. One warn per (qkc, holder, rejected) triple.
- **Stale demand reports are evicted**: the presence sweeper also calls
  `DemandRegistry::evict_older_than` every `presence_ttl_secs / 3` with
  `demand_ttl_secs` (default = presence TTL); a grade flip no longer leaves a
  ghost commodity (`dup_pairs` in `mcmcf.solve` should stay 0).
- **The data path has two keyed integrity layers since 2026-08-28** — before
  that it had none, and `key_digest` (an *unkeyed* SHA-256 that works only
  because it rides inside the encryption) was the whole story. OTP and the ORR's
  old HKDF-XOR are both malleable, so anyone who could touch the ciphertext
  could apply an arbitrary delta undetected.
  - **Hop by hop**: `frame_auth = off|prefer|require` per link puts an
    HMAC-SHA256 on `FRAME_RECV`/`FRAME_RELAY` (kinds `0x04`/`0x05`) and on the
    NOTIFY (`0x25`), covering the whole frame — identities, both cleartext
    headers, ciphertext. Root is `link_psk`; key is
    `HKDF(link_psk, salt = session)`. See `common/src/crypto/frame_mac.rs` and
    `qkc/src/frame_auth.rs`. Three things to keep in mind:
    - **Verify the MAC before touching the replay window.** The other order
      lets anyone reset the receiver's window with an invented `session`.
    - **Freshness lives inside the authenticated message, by design.** The
      trailer is `session ‖ counter ‖ tag`: `session` is a random incarnation
      per process run (same idea as the DKMS `incarnation`), without which a
      restarted sender's legitimate frames look exactly like a replay. The
      NOTIFY's first HMAC (2026-08-27) got this wrong — fixed `epoch = 0`, no
      counter — and was replayable; it now shares this trailer.
    - **A link with a MAC must be declared at BOTH ends**, like `pqc_auth =
      sign`: the root is local config the SDN neither carries nor should, so the
      end that learns the link from an announcement has no PSK and drops
      everything. `render_config.py` emits `link_psk`/`frame_auth` outside the
      qkd/pqc branch — it used to emit them only for pqc, which silently
      dropped them on qkd links.
  - **End to end**: each ORR onion layer is AES-256-GCM (`nonce ‖ ct ‖ tag`)
    with `key_id ‖ epoch_id ‖ max_hops` as AAD, so a QKC on the path — which
    sees the onion in the clear between decrypt and re-encrypt, and recomputes
    the link MAC itself — cannot alter it, move it to another epoch, or change
    its `max_hops`. A diverging `master_secret` is now an error in `peel`
    instead of garbage delivered to the DKMS.
  `key_digest` is gone from the generator path (see the e2e entry below);
  `recv_corrupt` in `generator.state` now counts AEAD failures. **SAE session keys still have no
  end-to-end check of their own**: they are OTP-wrapped with a transport key and
  delivered over ETSI-020, so if a transport key were ever wrong on one side the
  two SAEs would silently get different keys. The layers above make that
  unreachable in practice, not impossible in principle.

- **The QKC must propagate `frame.epoch_id` byte-for-byte** (fixed
  2026-08-30, `qkc/src/relay.rs::{local_deliver_frame,outbound_frame}` +
  test). That field is the ORR's: which `master_secret` epoch peels the
  onion. The relay rebuilt every frame with `Frame::empty` and never copied
  it, so every onion arrived as epoch 0 — invisible while the ORR never
  rotated (everything WAS epoch 0), and the reason the rotation looked
  broken in 2026-08 ("frames con epoch=0 que el receiver dropea"). Measured
  on the first live rotation: `peel_failed` ≈ 25 % and passive re-bootstraps
  in a loop. Symptom if it regresses: `orr.incoming handle_failed error=aead`
  right after the first `orr.rotation committed`.
- **ORR `master_secret` rotation is live since 2026-08-30** (`rotation.rs`,
  default `rotation_period_ms = 3600000`; it was disconnected before). Four
  invariants, each the fix of a measured bug: the initiator stores epoch N
  **before** sending the FIN and switches its send epoch only on the `ok`
  (`send_epoch_for`: confirmed epoch on the initiator, latest installed on
  the responder — the 1-RTT race); any bootstrap resets the whole epoch
  history on both sides (`reset_for_bootstrap`); one establishment in flight
  per pair (`try_mark_rebootstrap_inflight` per attempt) and one rotation
  task per pair (`try_mark_rotation_spawned`); and a peer answering «no
  bootstrap_secret» has restarted, so the rotation loop re-bootstraps it
  itself (`bootstrap::rebootstrap`) — with `max_hops = 0` nothing else would.
- **The end-to-end layer on transport material lives in the DKMS since
  2026-08-28, and `default_max_hops` is `0`** (`dkms/src/e2e.rs`). The ORR
  onion sealed ORR_A↔ORR_B, so ORR_B handed its DKMS the key in the clear —
  one hop short of the relationship's endpoint, which in multi-host is another
  process — and the whole ORR↔ORR bootstrap (with its two open bugs) sat on
  the critical path of every key. Now `emit_key` seals each `DKMS_BUFFER`
  with AES-256-GCM under a per-pair ML-KEM secret agreed over the existing
  ETSI-020 mTLS (`POST /kmapi/v1/e2e/kem`), and `handle_orr_delivery_buffer`
  opens it **before** touching anything. The ORR is a relay; onion modes
  (`1`, `≥2`, `-1`) still work as optional path privacy on top of an
  already-sealed payload. Four things to keep in mind:
  - **Tag, epoch and counter ride in `header_dkms`, never in the payload.**
    The payload stays exactly 32 B, so the OTP chunker spends the same QKD
    material as before (the −51 % trap above). `E2e::seal` uses
    `aead::seal_detached`; the nonce is HKDF-derived from `key_id` alongside
    the key, so each key meets exactly one nonce.
  - **Open before acting on the header.** The AAD covers the whole
    `header_dkms` plus origin and destination. `incarnation` wipes the peer's
    buffers and `ack_endpoint` redirects ACKs; before this, with passthrough,
    a transit QKC could forge either. `note_peer_incarnation` now runs after
    `open`, and the e2e tests assert a forged incarnation fails the tag.
  - **Epochs are random, assigned by the responder, and carried per key.**
    Concurrent agreements yield two epochs, never one epoch with two secrets
    (the ORR's 2026-08-02 bug class). Any side may request — the channel
    authenticates both — so a restarted DKMS asks when it first emits and a
    receiver asks when it sees an epoch it lacks (`recv_no_epoch`, rate-
    limited to one attempt per 2 s per peer); the responder switches its
    send epoch to the new one, so both directions heal without any
    "passive re-bootstrap". Time rotation (`transport_e2e.rekey_secs`,
    3600) is driven by the lex-smaller peer only, over request/response, so
    it cannot stall on an idle link like the QKC rekey did.
  - **Hard cut, no compatibility mode.** A receiver without `e2e_*` headers
    drops the key as corrupt. Mixing an old sender with a new receiver would
    store ciphertext as key material and fail silently at the SAE — so all
    DKMSs upgrade together. Config lives under `[transport_e2e]` (suite,
    rekey_secs, epoch_history_keep, replay_window), all defaulted.
  Diagnostics: `generator.state` carries `e2e_epoch` (`none` sustained =
  agreement not converging: check the peer's HTTP endpoint and CA),
  `recv_no_epoch` and `recv_replayed`.

## Roadmap and measured state

**2026-09-03 — campaña de 60 celdas en CESGA (6 topologías × N=10..100 × 3
cargas).** Informe en `tests/results/campaign-2026-09/ANALYSIS.md` (gitignored)
y publicado en la web (`web_dkms`, `public/results/campaign-2026-09.html` y
`public/resultados/campana-2026-09.html`). Lo que hay que recordar de ahí:
- **Limpio en las 60**: 0 `recv_corrupt`, 0 `peel_failed`, 0 panics, 0
  intercambios ETSI-014 con bytes distintos, de N=10 a N=100.
- **El sello por frame rechazaba frames legítimos** en los enlaces más
  cargados (hasta 542 598 con `bad_mac = 0`) porque MAC + ventana anti-replay
  se comprobaban en las tareas concurrentes del dispatcher. Arreglado en
  `67ffcc5` (verificación en el lector, en orden de llegada) y remedido a 0.
  Ver el gotcha correspondiente arriba.
- **La estrella está limitada por su hub desde N≈40**: la cola de entrada del
  QKC (8192) desborda bajo saturación y los emisores expiran las claves sin
  ACK (21,7 M a N=100). Es estructural: un solo QKC en el camino de todos los
  pares. Candidatos: contrapresión adaptativa generador→ORR y cola por peer.
- **Equidad**: el peor par recibe el 1 % de un reparto uniforme en la RGG
  desde N=30 (Jain 0,07). Sin inanición, pero es el hueco que la ponderación
  por SAE (`w_k` del allocator `num`) tiene que cerrar — pendiente 3 de esta
  lista, ahora con datos.
- **El arnés satura el nodo a N≥70** en las familias densas (100–127 k
  claves/s servidas, carga media 500–700 en 64 cores): esas celdas miden la
  máquina, no la fibra.

What remains, in the order worth attacking it:

1. ~~Proxmox testbed run~~ — **CORRIDO 2026-08-31/09-01** con HEAD, PKI
   ML-DSA reemitido y LOS FLIPS PUESTOS (`ack_transport: etsi020` +
   `ack_socket_listen: false` + `bootstrap_trust: strict` + `served_dkms`).
   Resultado: t10 6/6 bytes idénticos, t11 7/7 bordes, t20 **950 claves/s
   sostenidas** (0 corruptas, 0 5xx, 0 panics), t30 34/35 (el nodo D se
   despliega e integra en caliente), t31/t50 verdes, t40 10/11, t41 **0
   desincronizados** tras el fix. Cazó UN BUG REAL: el sello
   per-época descartaba frames de épocas desconocidas SIN pedir resync — el
   sanador de 2026-08-30 estaba cableado al camino de claves y el sello
   nuevo se lo comía; tras reinicios cruzados un par quedaba mudo para
   siempre con el multipath enmascarándolo (49,8 % de pérdida, `qkc.notify:
   rechazado` en bucle). Arreglado (`frame_auth.open` → `request_resync`).
   Harness aprendido: el curl/python de las VMs (OpenSSL 3.0) no carga
   claves ML-DSA — el cliente SAE corre en el portátil (`SAE_CLIENT=local`
   en lib.sh) o vía `sae_load` (rustls); y el ORR necesita `./certs`
   montado en su contenedor (site.yml viejo no lo tenía). **Los flips
   quedan VALIDADOS en multi-host real: el cambio de defaults en el binario
   ya solo espera la decisión.** Falta el soak ≥ 24 h allí.
2. **Long soak** — nothing deliberate has run longer than ~50 min. An
   accidental 15 h idle soak (3-node PQC ring, 2026-08-24 night) already
   caught the time-based rekey misbehaving: `pqc_rekey_secs = 3600` produced
   **one** rotation in 15 h instead of ~14, synchronized across all three
   initiators at +3 h 45 (epoch 7 at 21:50:05 on every link), then nothing —
   with zero reconnects, zero resyncs and `taken` frozen, so neither volume
   nor the documented triggers explain it. Best-fit hypothesis:
   `establish()` hangs on an idle link (INIT or RESP not delivered until
   some unrelated event flushes it), so each rotation stalls inside the
   loop that owns it; the link stays healthy otherwise (0 corrupt, morning
   traffic fine over old epochs) — it is a forward-secrecy cadence bug, not
   a data-path one. Reproduce with a mocked `RekeyClock` or a short
   `pqc_rekey_secs` under idle before trusting any multi-hour run. Presence
   expiry under jitter and slow leaks remain unexercised.
   **Update 2026-08-30**: the cadence bug is fixed (timer hoisted out of the
   `select!`, `establish` bounded — see the QKC gotcha) and a deliberate
   60-min local soak on HEAD passed with every layer rotating at its 120 s
   knob: 30 QKC time-rotations per initiator link (`since_last_s` max 120,
   0 relinks), 30 ORR rotations per pair (0 failures, 0 re-bootstraps,
   `peel_failed=0`), ~50 e2e epochs per node, ACKs over ETSI-020 with the
   socket listener off (`ack_send_failed=0`), RSS flat after the fill, 6/6
   identical at the end (`tests/results/soak-local-20260830/ANALYSIS.md`).
   What remains is the ≥ 24 h run on the testbed with real network latency.
3. **Rate allocation quality** — λ lives at 0 most of the time and the
   per-peer rate signal oscillates (68 % zero samples in the CESGA stress);
   the system works because the λ·R fill term is sane while buffers fill
   (127 handed vs 126–130 observed) and the token bucket ceiling carries the
   rest. Investigated 2026-08-25; the zeros decompose into three states the
   logs can now tell apart (see the `mcmcf.solve` per-solve line):
   buffers-full idle (short-circuit r=δ≈0 — correct by design, inflates the
   percentage), λ crushed globally under load by the `1000·Σσ` penalty when
   a few local edges saturate (σ eats δ on those commodities → true zeros,
   and η=0 whenever microlp's phase-2 fallback fires, `eta_fallback` in the
   line), and phase-1 hard failure (`zero_rate_fallback`: r=0 for everything,
   not even drain compensation). The `drain_positive=0` thread is closed:
   the DKMS tracker/EWMA works (verified live end to end: record → report →
   solver → `/rate`); `drain_positive` counts `> 0.0` strictly and the EWMA
   never returns to exactly 0.0 after the first request, so the observed 0
   meant "no SAE request had ever reached those DKMSs" — generator load, not
   SAE load. Attribution measured on CESGA 2026-08-25 (jobs 9265640 +
   9266817, ring QKD N=10, arm A — `tests/results/cesga-9265640/
   ANALYSIS.md`): 95-96 % of the zeros under load are the solver denying
   commodities with REAL measured demand (δ 300-800 keys/s, buffer with
   headroom); idle accounts for 4-5 %; "no demand measured" for none. λ=0
   in 161/165 solves, `fill_total=0.0` throughout load. σ eats 42-71 % of
   drain_in — the aggregate is right (`drain_delivered` ≈ served, 15 022 vs
   14 466 at t1) but its distribution is not: phase 1 minimises **Σσ**
   (utilitarian), so the vertex solution starves whole commodities instead
   of sharing the shortage — per-pair zero fraction is bimodal (median
   13 %, p75 67 %, several pairs at **100 % for the whole 300 s**, worst
   ones sourced at the degree-2 nodes whose incident cut saturates). The
   max-min property the design claims is violated under overload.
   **Fix IMPLEMENTED 2026-08-25** (see "Solver & fairness" for the new
   architecture): production rates now come from `sdn/src/rates_num.rs`
   (`rate_allocator = num` default — proportional-fair prices, mathematically
   starvation-free; `maxmin` — exact waterfilling; `lp` — the old path as
   oracle), the LP gained a phase-0 `max t` floor so even in `lp` mode no
   commodity drops below the common served fraction, λ·R-global died as a
   semantics (fill is now lower-priority demand, `num_fill_weight`), and PQC
   edges have real declared capacity. Verified locally (unit tests: no
   starvation under 8:1 overload, proportional convergence, waterfill
   hand-cases, LP floor 75/25; live 3-node mesh: `mcmcf.num` shows
   `starved=0` with demand, sane fill at cold start). **VALIDATED on CESGA
   2026-08-25** (jobs 9274893 num-QKD, 9276866 num-PQC 32G, 9276909
   maxmin-QKD — `tests/results/cesga-9274893/ANALYSIS.md`): zeros-with-real-
   demand ≈ 0 in all three (only a ~10 s /rate-cache transient at each
   load onset), all 90 pairs at 0 % zeros, `starved=0` across every captured
   tick, PQC rates bounded by declared capacity (max 510-1815, not 8e8) with
   27 002 keys/s at t4. The measured fairness spectrum at t1: LP-vertex
   14 466 > num 13 323 (−7.9 %) > maxmin 11 941 (−17 %), with maxmin's
   worst pair guaranteed ≥35 % of its drain (`min_drain_frac`) where the
   baseline gave it 0 % for 300 s — the aggregate the old vertex "won" was
   bought by starving expensive pairs. Campaign-infra traps fixed on the
   way: `-d singleton` in cesga.sbatch (two mesh jobs sharing a node cross-
   connect over the fixed 127.0.0.1 ports — 520k SSLCertVerificationError),
   stress.sh's attribute() now parses /rate JSON (the old regex counted
   per-grade sub-objects: a structural 33 % zero floor), and PQC-only at
   N=10 needs `--mem=32G` (touches 16 GB). The two latent defects from the
   investigation are closed: `DemandRegistry::evict_older_than` IS wired
   into the presence sweeper since 2026-08-30 (see the gotcha above;
   `demand_ttl_secs = 0` now means "no expiry" instead of evicting every
   report), and the demand tracker keeps working as verified. Routing never
   depended on any
   of this.
4. **LP backend** — DONE 2026-08-25: `highs` wired and default
   (`SDN_SOLVER`), microlp/clarabel selectable; needs `cmake` to build
   (Dockerfiles + cesga.sbatch updated). The whole sdn test suite runs green
   through highs, phase 2 included. What remains is watching `eta_fallback`
   stay false in a real campaign.
5. **Adaptive back-pressure generator→ORR** — `max_emits_in_flight` is an
   explicit but static bound. Raising `max_tokens_per_peer_per_tick` beyond
   ~×10 still risks the Arm-B OOM pattern (queues grow unbounded at 16.6 GB).
6. **Known residual gaps** — as of 2026-08-30 three of the four are closed:
   responder-side-only epoch divergence (the responder now asks, 0x28);
   ORR re-bootstrap on single restart (rotation-driven); two ORRs/DKMSs on
   one QKC (the second is rejected, `Upsert::Rejected`). What remains: the
   SAE session key has no check the SAE itself can make (it trusts its KME;
   `session_key_digest` covers the DKMS↔DKMS leg), and the plain ACK socket
   is still the default transport until the testbed validates `etsi020`.
7. **Scale beyond ~20 nodes** — needs the split already prepared by the
   routing/rates decoupling: hierarchical areas for routing, distributed dual
   decomposition (NUM/price-based) for rates, BGP-style inter-domain for a
   global net. Design discussion only; nothing implemented.

## QKD rate estimation (in situ, 2026-09-01)

`r0/alpha/distance_km` are simulator parameters; real deployments can't know
them, so the QKC measures the link's generation rate through the only thing
every KME exposes — ETSI-014 `/status` (`stored_key_count`) — plus the
deliveries it already counts, and reports it in its announce. Design and all
rationale in `qkc/src/rate_estimator.rs`; live harness in
`scripts/test-rate-estimator.sh` (2 QKC + quditto, no data traffic; validated
2026-09-01: 0.2–1.5 % error, −50 % step tracked in ~12 s, pause/block modes).
The pieces, each the fix of a measured failure:

- **Conservation with censoring**: `produced = ΔS + drained`. Intervals where
  the stock touched `max_key_count` are censored (the KME discarded or — real
  hardware, `quditto --full-mode pause` — stopped distilling: production is
  invisible there and NO estimator can recover it). When everything is full
  and idle, the estimate **freezes as `floor`** — never decays to 0 from lack
  of data, which is what would close the deadly loop
  measure→SDN→rates→traffic→measure.
- **Peer drains count at NOTIFY arrival**, not at our `dec_keys` completion
  (the DEC worker runs seconds behind during fills: −15 % flat, measured).
  Boot caveat: the LATER-booting end over-reads a few % (the peer's
  pre-connection NOTIFYs arrive late via the reconnect queue); the SDN's
  min-combine picks the clean end.
- **Time-weighted mean over a 30 s horizon, NOT a median of window rates.**
  Block delivery (1.28 s cadence) sampled at 1 Hz makes window rates bimodal
  (256/s or 128/s); a median picks the mode: +27 % flat, measured. The
  Σkeys/Σsecs mean is alias-immune by construction. Level changes truncate
  the horizon (short-vs-long deviation sustained 3 windows down / 6 up).
- **Banking** (`keystore.rs::bank_from`): when the KME stock crosses ¾ of its
  ceiling and the ENC ring has headroom beyond 512, the poller pulls batches
  downstream. Material the ceiling would have destroyed gets banked usable,
  and the link stays in the observable band — the live rate step was only
  measurable because of this.
- **SDN side**: the measured rate travels in the announce OUTSIDE `EdgeMeta`
  (own fields in `QkcLinkAnnounce`), lands in a side registry
  (`TopologyStore::measured`, outside the snapshot like `presence`), and only
  the min-of-endpoints crosses into `EdgeMeta::measured_keys_per_s` behind
  the same 5 %/0.5 hysteresis as `/link-capacity`. Three rules, all tested:
  measurement noise must never bump `topology.version`; two endpoints
  measuring differently is NOT an `edges_conflict`; a heartbeat without
  measurement (restarted QKC, estimator still cold) must not erase the
  applied value (`recompute_edge` preserves it — the field is `serde(skip)`
  so declarations can't carry or clobber it).
- PQC links: nothing physical to measure; `stock()` returns `None` and the
  estimator never engages. `capacity_keys_per_second()` stays the single
  decision point (routing's bottleneck now goes through it too).

## Solver & fairness

- **Rates come from `sdn/src/rates_num.rs` since 2026-08-25** — three
  allocators behind `rate_allocator` in config / `SDN_RATE_ALLOCATOR` env:
  - `num` (DEFAULT): dual-decomposition prices over the FIXED WCMP fractional
    routing. `x_k = (w_k/Σf·μ)^{1/α}` with α=1 (proportional fairness:
    log-utility ⇒ a commodity with demand mathematically cannot starve),
    `μ_e` updated by clamped normalized gradient, published rates projected
    to feasibility every tick, price state persisted across recomputes and
    reconciled on topology churn. `w_k = δ_k + num_fill_weight·R_k/T` — the
    proactive fill is lower-priority demand now, NOT a global λ multiplier.
    It is the distributed algorithm of roadmap item 7 computed where the
    information already lives (the SDN); moving it onto the nodes is a
    transport change, not an algorithm change.
  - `maxmin`: exact progressive-filling waterfill (lexicographic max-min,
    the paper's promise) on the same fixed fractions — pure arithmetic, no
    LP. Drain first, fill on the residual.
  - `lp`: the MCMCF-λ two-phase LP, kept as oracle/reference, now with a
    phase-0 `max t` common-fraction floor that bounds σ_k ≤ (1−t*)·δ_k so
    its simplex vertex can no longer starve whole commodities.
  All three emit `McmcfSolution` and share `into_mcf_snapshot` (single
  publication path for `/rate` and the forwarding push). num/maxmin bypass
  the debouncer (they are microseconds; the debouncer exists because the LP
  is expensive) and run at `mcf_period_ms` cadence.
- The old **hybrid HIGH/LOW tier solver was deleted long ago** (mcf.rs is
  wire-types only) — tier semantics, if ever revived, map onto NUM utility
  weights, not onto a separate solver.
- **MCMCF-λ multipath** (k-splittable, WCMP) lives in the QKC
  `ForwardingTable`, **not** in the ORR (ORR is a relay, `max_hops=0`; the
  E2E seal is the DKMS's, see the e2e gotcha). An
  earlier attempt that put source routing in the ORR was abandoned. Since
  2026-08-24 the WCMP tables are derived from topology+capacity
  (`wcmp_from_topology`), not from the LP's edge flows; the rate mechanism
  (whichever allocator) owns **rates only**. See "Routing is decoupled".
- **Per-SAE fairness** (max-min by SAE count + EWMA demand) designed but not
  implemented — its natural home is now per-commodity weights `w_k` in the
  `num` allocator.
