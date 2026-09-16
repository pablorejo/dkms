# Container deployment (multi-institution, manual)

Public Docker images, one per module (`qkc`, `orr`, `dkms`, `sdn` and the
`quditto` simulator), so that **each institution deploys its own part
autonomously**: `docker compose up` filling in only a short `node.yml`. No
central orchestrator.

> **In a hurry?** [`examples/quick_start.md`](examples/quick_start.md) has
> just the commands, no explanations. This guide also tells you **what each
> step does, what each field means and how to verify that it works**. The
> ten-minute overview of the model is [docs/deployment.md](../docs/deployment.md);
> the map of every document, [docs/README.md](../docs/README.md).

The network model: a **single central SDN** (run by the operator) that sees
the whole topology, and at each node/institution a trio **QKC + ORR + DKMS**
(on the same machine or spread out):

```
            ┌──────────── SDN (central operator) ────────────┐
            │  sees the topology, allocates the rates and    │
            │  pushes forwarding to the QKCs                 │
            └──┬──────────────────┬──────────────────┬───────┘
   institution 1                  │                  │
┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐
│ DKMS-1 (keys to  │   │ DKMS-2           │   │ DKMS-3           │
│  SAEs, ETSI-014) │◄──┼─►ETSI-020 mTLS◄──┼───┼─►                │
│   │              │   │   │              │   │   │              │
│ ORR-1 (relay of  │◄──┼─►ORR-2 (gRPC)◄───┼───┼─►ORR-3           │
│  sealed keys)    │   │   │              │   │   │              │
│   │              │   │   │              │   │   │              │
│ QKC-1 (QKD/PQC   │◄──┼─►QKC-2 (TCP)◄────┼───┼─►QKC-3           │
│  links)          │   │                  │   │                  │
└──────────────────┘   └──────────────────┘   └──────────────────┘
```

- **Without real QKD** → the QKC uses **PQC** links ("QKD simulated by PQC",
  ML-KEM-768 with periodic re-keying). No quditto needed.
- **With real QKD** → the QKC's link points at the hardware's **ETSI-014
  KME**.

Flow validated end to end on the Proxmox testbed on 2026-08-31/09-01 with
every current security default on (mTLS control plane, link MAC, e2e seal,
ETSI-020 ACKs, `bootstrap_trust: strict`): identical ETSI-014 keys at both
ends, 950 keys/s sustained, a node added hot
([engineering notes, roadmap item 1](../docs/engineering-notes.md#roadmap-and-measured-state)).

## How an image works inside (read this once)

Each image contains the Rust binary of its module + a common entrypoint:

1. The container starts with `ROLE` set (qkc|orr|dkms|sdn|quditto).
2. The entrypoint looks for `/config/node.yml` (the one you mount) and turns
   it, with `render_config.py`, into the binary's native config — a TOML —
   written to `/run/cfg/`. The SDN carries no topology: it infers it from
   the announcements.
3. It starts the binary pointing at that config.

Every node.yml (qkc/orr/dkms/sdn) also has an escape valve, `extra:`: a
free-form map merged into the rendered TOML, for the fields of
`src/config.rs` that have no key of their own (e.g. `extra: {generator:
{max_tokens_per_peer_per_tick: 64}}` or `extra: {rate_allocator: maxmin}`).
Scalars go before the first table, one-level dicts into their table; a
collision with a key the renderer already emits is an error, not an override.

Practical consequences:

- **You only edit `node.yml`**; field names and default ports are set by the
  renderer (a mirror of each crate's `src/config.rs`).
- To **debug the actual config** the binary received:
  `docker compose -f <rol>.yml exec <rol> cat /run/cfg/qkc.toml` (qkc) or
  `.../run/cfg/default.toml` (the rest).
- **Escape hatch**: if you mount a raw TOML (`qkc.toml` for qkc,
  `default.toml` for the rest, in `/config`), the entrypoint uses it as is
  and generates nothing. Useful for configs that node.yml does not expose.

The compose files in `compose/` are deliberately minimal:
`network_mode: host` (no port mapping: the binary listens directly on the
machine), `restart: unless-stopped` (automatic restart after a reboot or a
crash) and the `node.yml` mounted read-only.

## Requirements

- Docker + `docker compose` on each machine (Debian, Raspberry Pi OS, etc.):
  `curl -fsSL https://get.docker.com | sudo sh`
- Connectivity between the machines that need to talk to each other (next
  table). Between institutions: public IPs, a VPN (WireGuard) or agreed
  routes.
- For every module: a node certificate signed by a **CA common** to the whole
  network (`net-ca`, issued with `docker/gen-certs.sh`; the TLS section of
  step 4 explains the planes). mTLS is the rendered default on every plane,
  and a module rendered with `[tls]` and no cert files does not start.
  `gen-certs.sh` creates the CA the first time and reuses it afterwards, so
  issue every cert from the same `certs/` (or copy `net-ca.crt` +
  `net-ca.key` there first): two directories give two CAs, and every mTLS
  dial then fails with `UnknownIssuer`.

## Ports: who connects to whom

With `network_mode: host` the ports are opened directly on the machine. The
"who comes in" column is the one that matters for the firewall (the
plane-by-plane view, with what each port carries, is
[docs/ipc.md § Ports](../docs/ipc.md#4-ports)):

| module | port | protocol | who comes in |
|--------|--------|-----------|-------------|
| qkc  | 20000 (peer)   | binary TCP | the neighbour QKCs (both directions) |
| qkc  | 20001 (local)  | binary TCP | **its** ORR (localhost if co-located) |
| qkc  | 20002 (admin)  | HTTPS mTLS | **only the SDN** (forwarding push) |
| orr  | 20003 (grpc)   | gRPC mTLS | the peer ORRs and **its** DKMS |
| orr  | 20004 (metrics)| HTTP | Prometheus (optional) |
| dkms | 20005 (sae)    | HTTPS mTLS | the SAEs (ETSI-014) |
| dkms | 20006 (peer)   | HTTPS mTLS | the other DKMSs (ETSI-020, and the generator ACKs ride on it) |
| dkms | 20007 (grpc)   | gRPC | internal to the host: rendered on `127.0.0.1` |
| dkms | 20008 (metrics)| HTTP | Prometheus (optional) |
| dkms | 20009 (ack)    | plain TCP (legacy) | nobody by default — it only listens with `ack_socket_listen: true`, and then only the other DKMSs (socket ACKs) |
| sdn  | 19000 (grpc)   | gRPC mTLS | all the DKMSs (the ORR's optional channel does not come up under `control_tls`; the QKC has no gRPC client) |
| sdn  | 19002 (http)   | HTTPS mTLS | all the QKCs, ORRs and DKMSs (announcements, `/rate`) + admin |
| sdn  | 19010 (metrics)| HTTP | Prometheus (optional) |
| quditto | 20010 (http)| HTTPS mTLS ETSI-014 | the QKCs at **both ends** of the link |

The mTLS labels are what the renderer emits out of the box (`control_tls`,
`grpc_tls` and the quditto's `tls`, all on by default); the plaintext
variants exist only as explicit opt-outs (next section).

Minimum rules between institutions that link to each other: 20000, 20003
and 20006 between them (20009 only if you switched the ACKs back to the
legacy socket, at both ends); 20002 only from the SDN; 19000 and 19002 open
towards the SDN from all of them (the modules register on 19002 and the
DKMS polls its rate there). The `metrics` ports can stay closed.

Port override: a `ports:` block in the module's `node.yml` — only needed if
one machine runs **several nodes of the same role** (e.g. tests).

### Security and firewall

Read this; the full model is in [`docs/SECURITY.md`](../docs/SECURITY.md).

Strong authentication sits where the wire is crossed, and it is the default
on every plane: mTLS DKMS↔SAE (always), mTLS on the ORR's gRPC —DKMS↔ORR
and ORR↔ORR— (`grpc_tls`), mTLS on the control plane —SDN admin/gRPC,
announce, forwarding push— (`control_tls`, since 2026-09-03), the QKC↔QKC
link handshake signed with the node cert (`pqc_auth = sign`) plus the
link MAC (`frame_auth = require`) whenever the QKC has `[tls]`, and
mTLS on the quditto's ETSI-014; all with ML-DSA-65 node certificates from
`net-ca`. Each one has an explicit, written opt-out. The rest **depends on
these ports living on a trusted network**:

- **DKMS `grpc` (20007)**: operator plane with NO auth; its `Drain` RPC
  wipes every buffer in one call. It is rendered on **`127.0.0.1` always**,
  even if `listen_ip` is `0.0.0.0`; only `control_addr: <ip>` in the
  `node.yml` opens it, and then the DKMS warns about it at boot and it has
  to be firewalled to the internal network. Never between institutions.
- **DKMS `ack` (20009)**: plain TCP with no auth, **off out of the box since
  2026-09-03**: the ACKs go out over the ETSI-020 mTLS (20006, identity =
  cert; `ack_transport: etsi020`) and the socket listener does not listen
  (`ack_socket_listen: false`). Only during a migration with old peers does
  it go back to `socket` + `ack_socket_listen: true` at BOTH ends, and then
  20009 is opened only between the DKMSs that link to each other. It was
  the last unauthenticated cross-institution plane (validated on Proxmox
  2026-09-01: 950 keys/s sustained with the socket off).
- **QKC `local`**: intra-institution (same host as its ORR).
- **QKC `admin` (20002)**: the forwarding-table push comes in through it —
  whoever controls it decides which way every OTP frame travels. With
  `[tls]` in the QKC (`control_tls`, **on by default since 2026-09-03**) it
  serves mTLS with a MANDATORY client cert (net-ca), and an SDN with `[tls]`
  pushes `https` with its cert: the two go together (only one side off fails
  loudly on both). `control_tls: false` leaves it in the clear — trusted
  internal network only, and on ALL nodes at once. The ORR's `grpc` (20003)
  runs with mTLS by default; only if you switch it off (`grpc_tls: false` in
  the ORR and `orr_tls: false` in the DKMS) does DKMS↔ORR carry material in
  the clear, and then **they must share a host or a trusted L2 network**.
- **`metrics` (all)**: no auth and they answer on any path — internal
  network.
- **SDN `http`/`grpc` (19000/19002)**: mTLS **by default** (`control_tls`,
  2026-09-03; needs `sdn.crt`/`sdn.key` from `net-ca` in `certs_dir`) — in
  the clear (`control_tls: false`) anyone with network access can register
  nodes or rebind SAEs. With `[tls]` the SDN also pushes the forwarding
  tables over https (see QKC `admin`), and the modules announce themselves
  over `https://`: an `sdn_url`/`sdn_endpoint` with no scheme comes out as
  `https://` with `control_tls` on and `http://` with `control_tls: false`;
  a hand-written scheme always wins.

## Step 0 (maintainer): build and publish the images

**One person does it, once per version** — the institutions only `pull`.

```bash
# from the repo root, with buildx set up for multi-arch
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
```

What it does: compiles the 5 binaries **in a single pass** (the build stage
is shared by the 5 targets) and publishes
`tuusuario/{qkc,orr,dkms,sdn,quditto}:latest` for `linux/amd64` +
`linux/arm64` (Raspberry Pi). Bake variables: `IMAGE_PREFIX` (namespace on
Docker Hub) and `TAG` (default `latest`).

`quditto` is only deployed if you want QKD links **without hardware** (see
the simulator section further down); the other four are the runtime modules.

Build notes:

- `aws-lc-sys` (a rustls dependency) requires **gcc-12**; the
  `rust:1.88-bookworm` base already has it. The toolchain is pinned by
  `rust-toolchain.toml` (1.88) — if the build fails with "rustc X is not
  supported", the pin and the `Cargo.lock` have drifted apart.
- arm64 without an ARM runner goes through QEMU emulation (slow but works).

Variants:

```bash
# try a single arch locally, without push (leaves the images in the local docker)
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load

# lab without a registry: move images over SSH
docker save tuusuario/qkc:latest | ssh otra-maquina docker load
```

## Common layout of a deployment

Every module is launched the same way: a directory with 3 files + `certs/`
(every compose file mounts it: with mTLS on by default every role presents a
node cert).

```
mi-modulo/
├── <rol>.yml      # compose file of the role (copy it from docker/compose/, not edited)
├── .env           # IMAGE_PREFIX=<namespace>  (read by docker compose)
├── node.yml       # the ONLY thing you edit (templates in docker/examples/)
└── certs/         # <name>.crt/.key + net-ca.crt (docker/gen-certs.sh; the DKMS also sae-ca.crt)
```

Identical commands for the 5 roles:

```bash
docker compose -f <rol>.yml pull      # fetch/update the image
docker compose -f <rol>.yml up -d     # start in the background
docker compose -f <rol>.yml logs -f   # follow the logs (Ctrl-C does not stop the service)
docker compose -f <rol>.yml restart   # restart (re-reads node.yml)
docker compose -f <rol>.yml down      # stop and remove
```

**Recommended boot order**: SDN first and then the rest in any order (QKC →
ORR → DKMS gives the cleanest logs). It is not critical because everything
retries: the DKMS retries the SDN 30×1 s at boot, the ORRs re-bootstrap
their peers, and the SDN re-pushes the forwarding on every tick until every
QKC answers. One exception: the DKMS dials its ORR only at boot (20×1 s)
and, if it is not there, boots without a generator — start the ORR before
the DKMS, or restart the DKMS afterwards. The warnings of the first ~60 s
are transients of this dance; what matters is the steady state.

In the examples: SDN at `10.0.0.100`, node 1 at `10.0.0.11`, node 2 at
`10.0.0.12`. Substitute your IPs.

---

## 1. SDN (central operator)

**What it is**: the SDN, the single controller. It **infers** the global topology from
what the modules tell it when they boot, allocates the rates (the `num`
allocator by default; the MCMCF-λ LP stays as a reference) and pushes to
each QKC its forwarding table.

There is no out-of-band coordination: an institution does not tell the
operator its IPs so that they can be entered by hand. It deploys its modules
pointing at this SDN and they appear in the graph; if it shuts them down,
they disappear.

**Step 1 — directory and files:**

```bash
mkdir sdn && cd sdn
.../docker/gen-certs.sh sdn 10.0.0.100 ./certs   # control_tls (default on): its node
                                                 # cert, from the federation's net-ca
cp .../docker/compose/sdn.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.sdn.yml node.yml
```

**Step 2 — edit `node.yml`: almost nothing.** The SDN **carries no
topology**. It boots with an empty graph and builds it from what each module
tells it when it registers. Its `node.yml` only holds its own settings, all
with sensible defaults:

```yaml
# mcf_period_ms: 5000
# presence_ttl_secs: 90
```

| field | meaning |
|-------|-------------|
| `listen_ip` / `ports` | where it listens (19000 gRPC, 19002 HTTP, 19010 metrics). Override only if they clash. |
| `mcf_period_ms` | cadence of the rate recompute (default 5000). A topology change is picked up by the next tick; the forwarding tables are re-pushed at once, they do not wait for it. |
| `presence_ttl_secs` | a module that stops announcing itself for longer than this leaves the topology, with its links. It must exceed the modules' `sdn_announce_secs` (30 by default): that is 3 lost announcements. `0` disables it. |

**Step 3 — start and verify:**

```bash
docker compose -f sdn.yml up -d && docker compose -f sdn.yml logs -f
# the admin is mTLS by default (control_tls): any net-ca cert opens it, its own will do
S="--cert certs/sdn.crt --key certs/sdn.key --cacert certs/net-ca.crt"
curl -s $S https://localhost:19002/topology     # at first: everything at 0
```

As the modules boot, the graph fills in on its own:

```bash
curl -s $S https://localhost:19002/topology     # {"qkcs":2,"orrs":2,"dkms":2,"edges":1,...}
curl -s $S https://localhost:19002/qkcs
curl -s $S https://localhost:19002/links
```

Healthy logs and what they mean:

```
sdn::http_api: qkc registered qkc=1 added=["2"] pending=[]
    → a QKC registered; `pending` lists neighbours that have not booted yet
sdn::service: forwarding push done ... qkcs_ok=2 qkcs_err=0
    → the SDN reached the admin port (20002) of the 2 QKCs and pushed forwarding to them
sdn::service: MCMCF-λ recomputed n_commodities=2 n_edges=1 lambda=... flows_with_rate=2
    → the rate allocator ran (the line keeps its LP-era name);
      n_commodities = DKMS pairs × 2 directions
```

`qkcs_err>0` while the QKCs are not up is normal (it recovers on its own).
If it stays permanent, 20002 is filtered from the SDN or the QKC's
`advertise_ip` is wrong.

**Operation**: **there is no manual registration**. A new institution
deploys its modules with this SDN's `sdn_url` and appears on its own; if it
shuts them down, it disappears on its own once `presence_ttl_secs` elapses.
The SDN is never restarted because of a topology change.

If a module does not appear, look at its log: it will say
`anunciado a la SDN` (announced to the SDN) with `accepted`/`pending`, or
the error explaining why it cannot.

---

## 2. QKC

**What it is**: the node's key-material layer. It keeps a link with each
neighbour QKC — over real QKD hardware (it speaks ETSI-014 to the KME) or
over PQC (it derives material with ML-KEM and renews it periodically) — and
fills its per-peer keystores with it. No routing is configured on it: **the
forwarding table is pushed by the SDN** to the admin port.

**Step 1 — directory and files:**

```bash
mkdir qkc && cd qkc
.../docker/gen-certs.sh qkc-1 10.0.0.11 ./certs   # control_tls (default on): its node cert
                                                  # (announce, admin mTLS, signed link handshakes)
cp .../docker/compose/qkc.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.qkc.yml node.yml
```

**Step 2 — edit `node.yml`:**

```yaml
qkc_id: 1

sdn_url: "10.0.0.100"          # the SDN learns about this node on its own
advertise_ip: "10.0.0.11"      # IP through which the SDN reaches THIS QKC

links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"       # IP (or IP:port) of the neighbour QKC
    type: pqc
  # with a real QKD node:
  # - neighbor_id: 3
  #   neighbor_addr: "10.0.0.13"
  #   type: qkd
  #   kme_url: "https://mi-kme:443"
  #   r0: 2000                       # link model, for the SDN's solver
  #   alpha: 0.2
  #   distance_km: 5
```

| field | meaning |
|-------|-------------|
| `qkc_id` | numeric id of the node. |
| `sdn_url` | HTTP admin of the SDN (port 19002 if not given; the same key names the gRPC port 19000 on the ORR, whose HTTP admin URL is derived from it). The QKC announces itself and the SDN adds it to its topology. Omit it only for a QKC that must stay out of the topology; the SDN then never learns it, there is no manual registration. |
| `advertise_ip` | IP it announces itself with. Needed because the container binds `0.0.0.0`, which is no use to the SDN for calling it back. |
| `sdn_announce_secs` | how often it re-announces (default 30). It is also its heartbeat. |
| `key_size_bits` | size of the link keys (OTP, one per block of payload; the renderer writes 256 when omitted). **Must match at both ends of every link.** |
| `links[].neighbor_id` | id of the neighbour QKC. It is the only topology a QKC declares, and it is enough that **one** of the two ends does: the SDN builds the edge and tells the other. What is declared here is also a floor the SDN cannot remove. |
| `links[].neighbor_addr` | **optional** on `pqc` links if there is an `sdn_url`: the address does not travel in the announcement —the SDN already knows it, because every QKC announces its own— and comes back in the peer list. Omit it and the link is built when the SDN answers; declare it and it is built at boot, without depending on the SDN. On a `qkd` link it is mandatory (the SDN does not create those). Peer port 20000 if not given. |
| `links[].type` | `pqc` (no hardware) or `qkd` (with the `kme_url` of the ETSI-014 KME). **A `qkd` link must be declared, no matter what**: the SDN cannot invent your institution's `kme_url`, so if it offers one with no local config the QKC warns in the log and does not create it. |
| `links[].r0` / `alpha` / `distance_km` | physical model of the `qkd` link. The QKC does not use them: it passes them to the SDN, which sizes the edge with `r0·10^(−alpha·d/10)`. |
| `links[].capacity_keys_per_s` | declared capacity of a `pqc` link in keys/s (ignored on `qkd`). Undeclared, the SDN applies 10 000 — a finite default that makes the rate signal meaningful on PQC-only deployments too (they used to carry a 1e9 sentinel and `/rate` was noise). |
| `links[].pqc_*` | optional; on a `qkd` link with `[tls]` they govern the handshake that only roots the link MAC (`pqc_rekey_keys` is inert there): `pqc_suite` (default `ml-kem-768`), `pqc_rekey_keys` (rotates the secret every N keys, default 1000), `pqc_rekey_secs` (…or every T seconds, default 3600), `pqc_rekey_lookahead` (pre-derived epochs, default 2). |
| `links[].link_psk` | **legacy root** of the link's authentication: pre-shared secret, base64 of 32 bytes, IDENTICAL at both ends; HMAC of the handshake (`pqc_auth: prefer`/`require`), of the NOTIFY and —if `frame_auth` asks for it— of every data frame. With `[tls]` (the default) it is not needed: the handshake is signed with the node cert and the link MAC is rooted in the epoch secret that handshake establishes. Local config only: the SDN does not transport secrets, nor should it. |
| `links[].frame_auth` | `off` \| `prefer` \| `require`: per-data-frame MAC. **Omitted ⇒ `require` with `[tls]`** (the default; the root is the link's epoch secret, nothing to distribute) and `off` without it. See the section below. |
| `links[].pqc_auth` | `off` \| `prefer` \| `require` (HMAC with `link_psk`) \| `sign` (ML-DSA signature of the handshake). **Omitted ⇒ `sign` with `[tls]`** (the default: signed with the node cert and verified against `net-ca` + SAN, no per-pair material — the links the SDN creates come out signed too) and `off` without it. Applies to both link types: on `qkd` the handshake only roots the link MAC, the data keys stay the KME's. The legacy form of `sign` uses `sign_secret_seed` + `links[].peer_verify_key`. Like everything two-ended: identical on both. |
| `sign_secret_seed` | legacy ML-DSA seed (base64) of the QKC's signing identity, for `pqc_auth: sign` on a node without `[tls]`; with `[tls]` the node cert signs. Local config only. |
| `control_tls` | default `true`: renders `[tls]` (`certs_dir/<cert_name>.crt/.key` + `net-ca.crt`) — https announce, mTLS on the admin port, signed handshakes and sealed frames. `false` leaves all of that in the clear, and must be set on ALL nodes at once. |
| `cert_name` | name of the node cert in `certs_dir` (default `qkc-<id>`): the one it presents when announcing to an mTLS SDN, serves on the admin port, signs the link handshakes with and dials an https KME with. |
| `listen_ip` / `ports` | where it listens (20000 peer, 20001 local, 20002 admin). Override only if they clash. |
| `extra` | free-form map → TOML (see above). |

**On announcing itself.** An edge needs both of its ends registered, so the
QKC that boots first will see it as `pending` until its neighbour shows up:
the announcement is a loop, not a one-shot, and it converges on its own. A
re-announcement without changes does not touch the topology, so it triggers
no recomputes.

If the two ends declare **different** `r0`/`alpha`/`distance_km`, the SDN
keeps the first one that arrived and warns in the log; fix whichever
`node.yml` is wrong, because the link's capacity comes out of that number.

### Authenticating the link (`link_psk` + `frame_auth`)

The link payload is encrypted with OTP, which gives confidentiality and
**zero integrity**: XOR is malleable, so anyone who can touch the wire can
apply a delta to the ciphertext and the receiver decrypts a modified message
without noticing. And `sender_id` travels in the clear, unchecked. This is
closed by a per-frame HMAC-SHA256, with a counter and an anti-replay window
([`docs/SECURITY.md`](../docs/SECURITY.md) §Fase 8).

**With `[tls]` (the default) this is already on**, with nothing to
distribute: the handshake is signed with the node cert (`pqc_auth = sign`)
and the frames are sealed under the epoch secret it establishes
(`frame_auth = require`); the `qkc.frame_auth` line below shows it
(`mode=Require`, `signed`/`verified` climbing). What follows is the
**legacy PSK root**, for nodes running without `[tls]` (`control_tls:
false`).

**One `link_psk` per link is needed**, 32 bytes, identical at both ends and
distributed out of band — it is the same assumption QKD itself makes for its
authenticated classical channel. It is symmetric and 256 bits, so it is
quantum-safe (Grover leaves 128 effective):

```bash
openssl rand -base64 32     # one per LINK, not per node
```

```yaml
links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"
    type: pqc
    link_psk: "<openssl rand -base64 32>"   # the SAME on the neighbour; never the example's
    frame_auth: require
```

Three things to know before enabling it:

1. **A link with `frame_auth` must be declared at BOTH ends**, with the
   same PSK. The root is local config and the SDN does not distribute it, so
   the end that learns the link from an announcement is left without it and
   **drops everything that arrives** from that neighbour. It is the same
   case as `pqc_auth: sign`.
2. **Deploy in `prefer` before moving up to `require`.** A peer that does
   not yet understand authenticated frames ignores them silently, so going
   straight to `require` with one end not updated leaves the link dead. The
   safe order is: distribute PSKs → `prefer` everywhere → check → `require`.
3. **`require` without `link_psk` does not start**, on purpose: running
   unauthenticated while believing you are is exactly what the flag exists
   to prevent.

With the PSK set, the NOTIFY is always authenticated, even with
`frame_auth: off` (the mechanism, the trailer and the replay window:
[qkc/README.md § Authenticating the
link](../qkc/README.md#authenticating-the-link-handshake-and-per-frame-mac)).

To see it working, every 5 s and per link:

```
qkc::service: qkc.frame_auth me=1 peer=2 mode=Require signed=544746 verified=428948
              bad_mac=0 replayed=0 plain_ok=0 plain_rej=0
```

`bad_mac`, `replayed` and `plain_rej` must stay at 0. `plain_ok` climbing
with `prefer` means the other end does not sign yet — normal halfway through
the rollout, and what has to reach 0 before moving up to `require`.
`plain_rej` climbing with `require` is asymmetric config: someone is missing
the PSK. Cost measured on CESGA (n=10, QKD links, medium regime): −0.04 %.

**Step 3 — start and verify:**

```bash
docker compose -f qkc.yml up -d && docker compose -f qkc.yml logs -f
```

```
qkc::pqc_handshake: qkc.pqc.handshake.established me=1 peer=2 epoch=N
    → PQC link alive with the neighbour (one line per neighbour)
qkc::keystore: keystore.levels peer=2 enc=256 dec=256 taken=4096 misses=0
    → per-peer keystores filling/rotating; misses=0 = nobody asked for material
      that was not there
```

No `handshake.established`: the neighbour is down, its `node.yml` does not
declare this link, 20000 is filtered between the two machines, or its cert
hangs off another `net-ca` (the handshake is signed by default).

---

### The ORR gRPC runs under mTLS by default (`grpc_tls`)

The DKMS's transport material goes through the ORR's gRPC, and the ORRs of
the other institutions come in through it too (bootstrap). It runs with
**mTLS by default**, with the same node certificates from the network CA as
the other planes (ML-DSA-65 and hybrid X25519MLKEM768 exchange): the ORR
presents `certs/<orr_id>.crt` and **requires** a client certificate from
`net-ca`, and the DKMS talks to it over `https://` presenting its own. There
is nothing to enable; one certificate per ORR is needed
(`gen-certs.sh <orr_id> <ip> ./certs`, with `./certs` mounted at
`/config/certs` as in the DKMS). Without it, the ORR **does not start in the
clear on its own**: it stops and says what it is missing. It also says so
when it starts fine: `orr gRPC listening (mTLS)`.

Switching it off is a decision that has to be written down, at **both**
ends and on **all** ORRs at once —the peer addresses the SDN hands out
arrive as `http://` and each ORR upgrades them to `https://` according to
its own `grpc_tls`—, and it is only valid if the DKMS and the ORR share a
machine or a trusted internal network:

```yaml
# node.orr.yml
grpc_tls: false            # in the clear; [tls] then only comes out with control_tls (default on)
# node.dkms.yml
orr_tls: false             # dial http:// to the ORR
```

## 3. ORR

**What it is**: the material relay between nodes. It receives from its DKMS
each key already sealed end to end for the destination DKMS
(`dkms/src/e2e.rs`), feeds it into its node's QKC and, at the other end,
delivers it to the remote DKMS. With `default_max_hops` ≠ 0 it also adds
ORR↔ORR onion layers (path privacy), which are no longer needed for
confidentiality. It talks to: **its** QKC (20001), the SDN (19002 to
announce itself; 19000 only for multi-hop onion paths) and the ORRs of the
other nodes (20003).

**Step 1 — certificate.** The ORR's gRPC runs with **mTLS by default** (the
DKMS's transport material goes through it and outside ORRs come in through
it), so it needs its node certificate, signed by the same `net-ca` as the
DKMSs, with the advertisable IP in the SAN:

```bash
.../docker/gen-certs.sh orr_1 10.0.0.11 ./certs      # ML-DSA-65 by default
```

**Step 2 — directory and files** (with `./certs` inside):

```bash
mkdir orr && cd orr
cp .../docker/compose/orr.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.orr.yml node.yml
```

**Step 3 — edit `node.yml`:**

```yaml
orr_id: "orr_1"
qkc_id: 1
served_dkms: ["dkms-1"]
qkc_addr: "127.0.0.1:20001"
sdn_url: "https://10.0.0.100:19000"

peers:
  orr_2: 2
peer_grpc_addrs:
  orr_2: "http://10.0.0.12:20003"
```

| field | meaning |
|-------|-------------|
| `orr_id` | **mandatory convention**: `orr_<node id>` — the renderer derives a DKMS's `orr_id` from its `node_id` that way. |
| `qkc_id` | the node it belongs to. |
| `served_dkms` | the certificate identities allowed on `SendMessage`/`StreamDeliveries`: its own DKMS. Omitted, the renderer derives it from a `node_id` in the same file; with neither it warns and the ORR accepts any `net-ca` certificate on that surface. |
| `qkc_addr` | where **its** QKC is (local port 20001). `127.0.0.1` if co-located; the QKC's IP if it runs on another machine. |
| `sdn_url` | the central SDN, given as its gRPC port 19000 (the same key names the HTTP admin port 19002 on the QKC); the HTTP admin URL this ORR announces on is derived from it. The gRPC channel itself is only used by the onion modes and does not connect under `control_tls` ([orr/README.md § Ports](../orr/README.md#ports)). |
| `advertise_ip` | IP through which the SDN reaches this ORR. Set it and the ORR registers itself in the topology; without it the ORR is never in the graph and its peers must be seeded in `peers`/`peer_grpc_addrs`, and the ORR says so in the log at boot (`no sé con qué IP anunciarme`, "I don't know which IP to announce with"). |
| `sdn_announce_secs` | how often it re-announces (default 30). It is also its heartbeat. |
| `peers` / `peer_grpc_addrs` | **seed, optional**: the ORRs to start the bootstrap with before the SDN answers. The live list is sent by the SDN in the announce response, and a new ORR appears on its own. What you put here is also a floor the SDN cannot erase. Note these are not only the physical neighbours: the ML-KEM bootstrap of the per-pair `master_secret` is end to end and independent of the link topology. `peers` maps `orr_id → qkc_id`; `peer_grpc_addrs` maps `orr_id → URL` (20003). |
| `default_max_hops` | default 0 (relay, no ORR layer): the material already travels sealed by the DKMS. `1`, `≥2` or `-1` add ORR↔ORR onion on top (path privacy) and put the ORR↔ORR bootstrap on the critical path. |
| `grpc_tls` | default `true`: mTLS on its gRPC with `certs/<orr_id>.crt/.key` + `net-ca.crt`. `false` only if the DKMS and the ORR share a machine or an internal network (and then `orr_tls: false` in the DKMS). |
| `certs_dir` | default `/config/certs` (where the compose mounts `./certs`). |
| `cert_name` | name of the node cert in `certs_dir` (default the `orr_id` itself). |
| `bootstrap_trust` | `strict` (default since 2026-09-03) \| `tofu`: with `strict` the ORR requires the peer's pubkey announcement to be signed with its node cert (chain to `net-ca` + SAN `dkms://<orr_id>`) or to match `peer_verify_keys`; with `tofu` it accepts the first one that arrives (only for an ORR in the clear, `grpc_tls: false`, knowingly — with `strict` and no `[tls]` the ORR warns at boot that no bootstrap will succeed). Validated on Proxmox (t30, hot add). |
| `rotation_period_ms` | rotation of the ORR↔ORR `master_secret` (forward secrecy), default 3600000 (1 h). |
| `extra` | free-form map → TOML (see above). |

**Step 4 — start and verify:**

```bash
docker compose -f orr.yml up -d && docker compose -f orr.yml logs -f
```

```
orr::bootstrap: orr.peer_pubkey bootstrap ok local=orr_1 peer=orr_2 suite=ml-kem-768
orr::bootstrap: orr.bootstrap bootstrap_secret ok local=orr_1 peer=orr_2
    → `bootstrap_secret` established with that peer (pair of lines per peer)
orr::grpc_server: orr.stream_deliveries subscribed subscriber=dkms-dkms-1
    → its DKMS has connected and is listening for deliveries
```

**Known gotcha**: if you restart **only one** ORR, its peers' next rotation
towards it fails with `no bootstrap_secret` and the initiator of each pair
re-bootstraps it by itself (`orr.rotation: ... rehago el bootstrap`,
retried with a backoff of up to 30 s, at its next rotation attempt); with
`default_max_hops: 0` the material keeps flowing meanwhile. Why and when:
[orr/README.md § Restarts and broken
links](../orr/README.md#restarts-and-broken-links). Restart the peer ORRs
only if those lines never turn into `bootstrap_secret ok`.

---

## 4. DKMS

**What it is**: the visible face of the node. It serves keys to the SAEs
over ETSI-014 (mTLS, 20005), agrees keys with the other DKMSs over ETSI-020
(mTLS, 20006) and keeps in RAM per-peer buffers of transport keys that a
*generator* fills in the background at the rate the SDN dictates (the ACKs
of that flow come back over the same ETSI-020 on 20006; the legacy plain
socket on 20009 is off by default). It is the module with the most TLS
planes, so its certificate step is the longest.

**Step 1 — certificates.** Three mTLS planes, two CAs (`sae_client_ca`,
`peer_dkms_ca` and `control_plane_ca` in the rendered `[tls]`):

- **SAE plane (20005)**: the DKMS presents its server cert; the SAE presents
  a client cert, verified against **`sae-ca`**, from which the DKMS
  **extracts its identity** (SAN `urn:dkms:sae:<id>`, or CN/DNS with the
  bare id).
- **Peer plane (20006)**: mTLS between DKMSs; both validate against the
  common **`net-ca`**. The ETSI-020 calls, the e2e KEM agreement and the
  generator ACKs all ride on it.
- **Control plane (outbound: ORR 20003, SDN 19000/19002)**: mTLS towards
  its ORR (`orr_tls`) and the SDN (`control_tls`), **by default**; the DKMS
  presents this same node certificate and verifies theirs with `net-ca`
  (`control_plane_ca`, which falls back to `peer_dkms_ca`). The ORR and the
  SDN need their own (`gen-certs.sh orr_1 10.0.0.11 ./certs`,
  `gen-certs.sh sdn 10.0.0.100 ./certs`).

```bash
# generates/reuses the CA in ./certs and issues the cert of THIS dkms.
# The 2nd argument is this machine's advertisable IP: it goes into the cert's SAN
# (the peers verify it) and MUST be the same as advertise_ip in node.yml.
.../docker/gen-certs.sh dkms-1 10.0.0.11 ./certs
```

It produces **two roots** `net-ca.crt`/`net-ca.key` (nodes) and
`sae-ca.crt`/`sae-ca.key` (SAEs) — only the first time, afterwards it
**reuses** the ones it finds — and `dkms-1.crt`/`dkms-1.key` (signed by
net-ca) with SAN `URI:dkms://dkms-1, IP:10.0.0.11, IP:127.0.0.1, DNS:localhost`. See
[`docs/SECURITY.md`](../docs/SECURITY.md) §2.

**Multi-institution**: `net-ca` is the COMMON root of the federation (the
mTLS between DKMSs requires it); `net-ca.crt` is distributed to everyone and
**`net-ca.key` never leaves whoever signs**. `sae-ca` can be per
institution (each DKMS puts in `sae_client_ca` the CA of ITS SAEs). Keeping
them separate prevents a SAE cert from passing as a DKMS cert. The node
cert's file name must be exactly `<node_id>.crt`/`.key` — the binary looks
them up by that name in `/config/certs`.

**Step 2 — directory and files:**

```bash
mkdir dkms && cd dkms            # with ./certs inside
cp .../docker/compose/dkms.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.dkms.yml node.yml
```

**Step 3 — edit `node.yml`:**

```yaml
node_id: "dkms-1"
advertise_ip: "10.0.0.11"
orr_addr: "127.0.0.1:20003"
sdn_endpoint: "https://10.0.0.100:19000"
sae_bindings:
  sae_1: dkms-1

peers:
  dkms-2:
    endpoint: "10.0.0.12"
    orr_id: "orr_2"
```

| field | meaning |
|-------|-------------|
| `node_id` | **mandatory convention**: `dkms-<node id>`. Must match the cert's name. |
| `advertise_ip` | IP of this machine **reachable by the other DKMSs**: it is what the SDN hands them as this node's ETSI-020 endpoint (20006) —over which their ACKs come back— and it must be in the cert's SAN. If it is wrong, the generator's ACKs do not come back and `ack_pending` grows without end. (With the legacy `ack_socket_listen: true` it is also the announced `ack_endpoint`, 20009.) |
| `orr_addr` | its ORR (20003). `127.0.0.1` if co-located. |
| `sdn_endpoint` | gRPC of the SDN. At boot it is retried 30×1 s; if the SDN shows up later, restart the DKMS. |
| `orr_id` | id of the ORR this DKMS hangs off; optional, the renderer derives `orr_<n>` from `node_id` `dkms-<n>`. The SDN places it in the graph by it, and `orr_addr` will not do: it is an address, not an id. |
| `sdn_announce_secs` | how often it re-announces (default 30). It is also its heartbeat. |
| `peers.<id>` | **seed, optional**: which DKMSs to work with while the SDN does not answer, and a floor the SDN cannot erase. `endpoint` (IP, peer port 20006 by default) and `orr_id` (that peer's ORR, through which the material travels). When the SDN answers, it sends the `endpoint` and the `orr_id`; `max_hops`, `orr_path`, `security_level` and `sni` always stay local. |
| `security_level` | default for serving keys: `strict_qkd` (QKD-grade material only; fails if there is none), `qkd_prefer` (default: QKD if available, otherwise PQC), `no_worry` (whatever there is). The SAE can ask for a different level per request; this is the default. |
| `fill_rate` | fill floor of the generator in keys/s (default 0 = only what the SDN allocates). |
| `transport_e2e` | **no need to touch it**. Every transport key leaves sealed end to end for the destination DKMS (`dkms/src/e2e.rs`): ML-KEM-768 agreed over the same mTLS as the ETSI-020 (20006), AES-256-GCM per key, rotation every 3600 s. The only thing it requires is what the ETSI-020 already required: that the DKMSs reach each other on 20006 with `net-ca` certs. In `generator.state`, a sustained `e2e_epoch=none` means that agreement is not getting through. Tunable if needed (`transport_e2e: {rekey_secs: ..., replay_window: ..., epoch_history_keep: ..., suite: ...}` — the `suite` must be identical across the whole deployment). |
| `capacity_per_peer` | size of the per-peer transport-key buffer (default 4096) — the `B_k` the SDN's solver sees and the knob the campaigns raise. |
| `ack_transport` | `etsi020` (default since 2026-09-03: outbound ACK over the mTLS POST on 20006, identity = cert) \| `socket` (legacy plain TCP, mixed migration only; set it the same at both ends). |
| `ack_socket_listen` | default `false` (socket 20009 does not listen; the `ack_endpoint` is not announced). `true` only while there are peers that ACK over `socket`. |
| `control_addr` | **do not set it** unless you know why: it opens the `DkmsControl` operator gRPC (20007, `Drain` wipes every buffer, no auth) on that IP instead of `127.0.0.1`. |
| `extra` | free-form map → TOML (see above). |
| `sae_bindings` | **mandatory**: the SAEs this node serves (`sae_id: <node_id>`). It is the list every ETSI-014 request is authorised against (fail-closed) and the one announced to the SDN. Without it every SAE request gets 404 `UnknownSae`; the renderer and the boot warn about it. |
| `sae_authorization` | default `true`. `false` disables that authorisation (any `sae-ca` cert can ask for keys on behalf of any SAE) — only if the SAE→DKMS membership is purely dynamic via the SDN. |
| `certs_dir` | default `/config/certs` (where the compose mounts `./certs`). |

**Step 4 — start and verify:**

```bash
docker compose -f dkms.yml up -d && docker compose -f dkms.yml logs -f
```

```
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20005 plane="sae"
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20006 plane="peer-dkms"
    → both mTLS planes up
dkms::service: orr deliveries pump connected subscriber=dkms-dkms-1
    → connected to its ORR and subscribed to deliveries
dkms::control::generator: generator.state peer=dkms-2 enc=4096 dec=4096
                          ack_pending=0 emit_total=... observed_keys_per_s=...
                          sdn_rate_keys_per_s=...
    → the health line (every 5 s, one per peer): enc/dec = fill of the
      transport-key buffers; ack_pending → 0 in steady state;
      sdn_rate = rate the SDN allocates to that flow
```

Two boot messages that are **not** errors: `generator: sin ack_endpoint
anunciado` (with the default `ack_transport: etsi020` nothing is announced —
the peers acknowledge over the ETSI-020; the line only matters with the
legacy socket) and `ack_reaper: expired pending keys` during the first
minute (keys emitted before the peer was up). One that **is**: `orr
unreachable after 20 retries; continuing without it` — the DKMS waited
20 × 1 s for its ORR and gave up, so the generator never starts and no
transport material flows; bring the ORR up and restart the DKMS. (The DKMS
no longer talks to the QKC: a leftover `qkc_endpoint` in a raw TOML is
accepted with a warning and ignored.)

---

## QKD links without hardware: the quditto simulator

**When you need it**: only if you want `type: qkd` links and have no real
KME. If `type: pqc` is enough for you, deploy none of this — PQC links talk
to no API, the two neighbour QKCs derive the material between themselves
with ML-KEM.

**What it is**: a fake KME. It keeps a buffer of random keys that it refills
at

```
R(d) = r0 · 10^(−alpha·d/10)   keys/s
```

and serves them over ETSI-014, just as the hardware would. To the QKC it is
indistinguishable from a real KME: point its `kme_url` here and that is it.

**One quditto per link, not per node.** The QKCs at both ends point at the
**same** `kme_url`; that is how both obtain the same material (why, and
what it implies for the rate: [quditto/README.md](../quditto/README.md#what-it-is-for-and-what-it-is-not)).
One of the two institutions (or the operator) brings it up on a machine
both can reach.

```bash
mkdir quditto-1-2 && cd quditto-1-2
.../docker/gen-certs.sh quditto 10.0.0.50 ./certs   # mTLS by default (next paragraph)
cp .../docker/compose/quditto.yml .
cp .../docker/examples/node.quditto.yml node.yml   # edit r0/alpha/distance_km
echo "IMAGE_PREFIX=tuusuario" > .env
docker compose -f quditto.yml up -d
docker compose -f quditto.yml logs -f   # healthy: "minter started ... rate_kps=..."
```

**mTLS by default (2026-08-31).** The OTP pads travel over this ETSI-014:
quditto serves TLS (hybrid PQC + ML-DSA cert) with a MANDATORY client cert
from the `net-ca`, and the QKC presents its node identity to it when
`kme_url` is `https` (its `[tls]`, the same material as the announcement).
Certs: `gen-certs.sh quditto <ip> ./certs` mounted in `./certs`. `tls:
false` in its node.yml leaves it in the clear — ONLY if the consuming QKC
runs on the same host.

Check:

```bash
curl -s --cert certs/qkc-1.crt --key certs/qkc-1.key --cacert certs/net-ca.crt \
     https://localhost:20010/api/v1/keys/1/status
# {"source_KME_ID":"quditto", ..., "stored_key_count":8192, "key_size":256}
```

In the `node.yml` of **both** QKCs of the link it is enough to point at the
simulator (the renderer prepends `https://` if you write no scheme; explicit
`http://` only against a quditto with `tls: false`):

```yaml
links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"
    type: qkd
    kme_url: "10.0.0.50:20010"
    r0: 2000          # the same as the quditto's node.yml
    alpha: 0.2
    distance_km: 5
```

**The three values go in the QKCs too.** `r0`, `alpha` and `distance_km`
are declared here (the quditto uses them to generate at that rate) and in
the `links` block of the `node.yml` of **both QKCs** of the link, which pass
them to the SDN to size the edge in its solver. If they diverge, the SDN
allocates flow over a capacity the link does not deliver. The SDN warns in
the log if the two ends do not agree with each other, and keeps the first
one that arrived.

`key_size_bits` must match in the quditto and in both QKCs (256 by default in
`node.yml`; a raw `qkc.toml` that omits it gets the binary's 1024).

Escape hatch: if you mount no `node.yml`, the container starts with the
variables `QUDITTO_R0`, `QUDITTO_ALPHA`, `QUDITTO_DISTANCE`,
`QUDITTO_MAX_BUFFER`, `QUDITTO_KEY_SIZE_BITS`, `QUDITTO_LISTEN` and the TLS
ones (`QUDITTO_TLS=on|off`, `QUDITTO_TLS_CERT/KEY/CLIENT_CA`) from the
environment — without certs and without `QUDITTO_TLS=off` it does not
start, and says what it is missing.

---

## A whole site on one machine (`site.yml`)

The common case — the node's whole trio on a single machine, a single
compose file:

```bash
mkdir mi-nodo && cd mi-nodo
cp .../docker/compose/site.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
# node.qkc.yml + node.orr.yml + node.dkms.yml (as in sections 2-4) and certs/
docker compose -f site.yml up -d
```

Leave `qkc_addr`/`orr_addr` at `127.0.0.1` (co-located). The per-role port
ranges do not collide with each other.

## SAEs: certificates and ETSI-014 smoke test

A SAE is any client application that asks its DKMS for keys. It needs a
client cert signed by the common CA **with the identity in the SAN**:

```bash
.../docker/gen-certs.sh --sae sae_1 ./certs     # SAN URI:urn:dkms:sae:sae_1
.../docker/gen-certs.sh --sae sae_2 ./certs
```

> With another SAN format the DKMS does not extract the identity: the
> `enc_keys` answers 200 but the `dec_keys` at the other end gives
> `key not found`.

### SAE client requirements (read before blaming the DKMS)

The SAE plane is TLS 1.3 **only with the post-quantum hybrid exchange
`X25519MLKEM768`** and **ML-DSA-65** certificates. There is no classical
fallback in either: a client that does not offer that group does not
negotiate and sees a `handshake failure` (the DKMS logs it as
`tls handshake failed`), and since 2026-08-31 an RSA/ECDSA certificate does
not authenticate either — verification is ML-DSA-only unless the verifying node
starts with the migration opt-in `DKMS_TLS_ACCEPT_CLASSICAL_CERTS=1` (it
warns loudly: authentication stops being post-quantum). What works:

- **OpenSSL ≥ 3.5** under the client (`openssl version`; in Python
  `python3 -c 'import ssl; print(ssl.OPENSSL_VERSION)'`). `curl`,
  `requests`, strongSwan… will do if their OpenSSL is that one. Debian 13
  (trixie) ships it; bookworm (3.0) and CESGA (1.1.1g) do **not**.
- **rustls** with this repo's provider: `tests/loadgen` (`target/release/
  sae_load`, same CLI and CSV as `tests/testbed/sae_load.py`) is the
  reference client and does not depend on the system OpenSSL.

The `curl` commands below assume the former. The binaries check this
against themselves at boot (`tls_pqc: self-check OK`) and abort if they do
not negotiate the hybrid: a running node already guarantees it on its side.

API at `https://<dkms>:20005` (ETSI GS QKD 014):

| endpoint | what it does |
|----------|----------|
| `GET /api/v1/keys/<slave>/status` | stock and limits of the pair: `stored_key_count`, `max_key_per_request` (64), `max_key_size` (256), `min_key_size` (64)… |
| `POST /api/v1/keys/<slave>/enc_keys` | body `{"number":N,"size":bits}` → `{"keys":[{"key_ID","key"}]}`. The DKMS delivers the key to the slave's DKMS over ETSI-020 in the same call. Also `GET …/enc_keys?number=N&size=bits` (§6.2 of the spec; defaults 1/256) — which is what strongSwan uses. |
| `POST /api/v1/keys/<master>/dec_keys` | body `{"key_IDs":[{"key_ID":"…"}]}` → the **same** key, at the other end. Also `GET …/dec_keys?key_ID=<uuid>` (§6.4: a single key per GET). |

Complete exchange between two nodes (the proof that the network works):

```bash
C=./certs
# 1) sae_1 asks ITS dkms (node 1) for a key with sae_2
curl -s --cacert $C/net-ca.crt --cert $C/sae_1.crt --key $C/sae_1.key \
  -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
  https://10.0.0.11:20005/api/v1/keys/sae_2/enc_keys
# -> {"keys":[{"key_ID":"<uuid>","key":"<b64>"}]}

# 2) sae_2 collects that key at the dkms of node 2
curl -s --cacert $C/net-ca.crt --cert $C/sae_2.crt --key $C/sae_2.key \
  -H 'Content-Type: application/json' -d '{"key_IDs":[{"key_ID":"<uuid>"}]}' \
  https://10.0.0.12:20005/api/v1/keys/sae_1/dec_keys
# -> the same "key" => end-to-end OK
```

## Day-to-day operation

```bash
# update a module to the latest published image
docker compose -f <rol>.yml pull && docker compose -f <rol>.yml up -d

# change the config: edit node.yml and
docker compose -f <rol>.yml restart

# see the rendered config the binary received
docker compose -f <rol>.yml exec <rol> cat /run/cfg/default.toml   # (qkc: /run/cfg/qkc.toml)
```

### Adding a new institution

**The nodes already running are not touched.** You deploy the new trio with
its `node.yml`, it announces itself to the SDN, and the others find out on
their next heartbeat (≤ `sdn_announce_secs`, 30 s by default): the announce
response carries the list of peers that correspond to each module, derived
from the graph. The DKMS registers the new peer, the ORR starts its ML-KEM
bootstrap, and the QKC creates the link on the fly. The step-by-step
walk-through, with the log lines at each side, is
[auto-configuration.md § Adding an institution](../docs/auto-configuration.md#adding-an-institution).

Two things remain manual:

- **QKD links.** A `qkd` link needs the `kme_url` of that institution's KME,
  which the SDN can neither know nor invent, so it is declared in the
  `node.yml` of both ends. If the SDN offers a QKD link for which there is
  no local config, the QKC warns in the log and does not create it. `pqc`
  links do get created on their own.
- **Certificates.** The new DKMS needs a cert signed by the common CA, with
  its `advertise_ip` in the SAN.

**`node.yml` is a floor, not a snapshot.** Nobody removes the peers/links
declared locally: the SDN can add peers and remove the ones it added, but a
local peer stays — without that rule a boot in which the SDN lags behind
would cost live links and live key material ([peers ride back on the
announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement)).

Conversely, retiring an institution is automatic: it stops announcing
itself, `presence_ttl_secs` expires it from the graph (90 s by default = 3
lost announcements) and the others drop it on the next heartbeat. Tearing a
link down releases its `SecretStore`, so its material is wiped.

## Troubleshooting

The healthy log lines and counters of each module are in its README:
[QKC](../qkc/README.md#health-and-diagnostics),
[ORR](../orr/README.md#health-and-diagnostics),
[DKMS](../dkms/README.md#health-and-diagnostics),
[SDN](../sdn/README.md#health-and-diagnostics).

| symptom | probable cause |
|---------|----------------|
| `dec_keys` → `key not found` with `enc_keys` OK | SAE cert without SAN `urn:dkms:sae:<id>` (use `gen-certs.sh --sae`), or the destination DKMS does not know the sender yet (check whether the SDN has both of them in the graph) |
| curl → `alert certificate required` | the SAE's `--cert/--key` is missing, or its cert was not signed by the CA the DKMS mounts in `certs_dir` |
| SDN `qkcs_err>0` permanently | the QKC's `advertise_ip` is wrong, or 20002 is filtered from the SDN |
| ORR `no bootstrap_secret` / "sin master_secret" after restarting a single ORR | its initiator re-bootstraps it on its next rotation attempt — look for `orr.rotation: ... rehago el bootstrap` then `bootstrap_secret ok` ([orr/README.md](../orr/README.md#restarts-and-broken-links)). Restart the set only if that never comes |
| QKC without `handshake.established` | neighbour down, link not declared at the other end, or 20000 filtered |
| PQC link alive but no keys, `keystore.levels … enc=0 dec=0 taken=0` + `timeout waiting pqc-secret` | you restarted one end and the re-negotiation did not fire. The end with the numerically **smaller** `qkc_id` must log `qkc.pqc.relink` as soon as the other one reconnects; if it does not show up, the outbound socket did not drop (a NAT/proxy keeping it open?). Restarting the smaller end fixes it |
| DKMS `ack_pending` grows without end | the peer cannot reach my 20006 (the ACKs come back over ETSI-020 by default; its `generator.state` shows `ack_send_failed` climbing) or my `advertise_ip` is wrong (the SDN hands it to the peers as my endpoint). With the legacy socket: its 20009 is filtered. Read `generator.diag` — it says which it is |
| DKMS `orr unreachable after 20 retries; continuing without it` at boot | its ORR was not up within 20 s of boot (`orr_addr` wrong, 20003 filtered, or `orr_tls`/`grpc_tls` and the CA do not match — the log carries the cause); the generator does not start. Fix it and restart the DKMS |
| `enc_keys` slow or failing towards a peer | the local ORR does not have that peer yet: either the SDN has not sent it (are both announcing themselves?), or its ML-KEM bootstrap has not finished — look for `orr.peer_pubkey bootstrap ok` for it |
| the config does not match what you expected | look at the rendered one: `exec <rol> cat /run/cfg/…` |

### Diagnosing the DKMS-to-DKMS key cycle

The path has four hops —I emit → it travels over ORR/QKC → the peer stores
it → its ACK comes back over ETSI-020 (a POST to my 20006, mTLS; the legacy
plain socket on 20009 only with `ack_transport: socket`)— and the cycle
only closes if all four work. Every DKMS dumps one `generator.state` line per peer every 5 s
with one counter per hop:

```
peer=dkms-2 enc=… emitted=… recv=… ack_sent=… acked=… expired=… ack_miss_peer=… ack_miss_key=…
```

It reads left to right; the first zero is the broken hop. Note that
`emitted`/`acked` belong to MY side (I generate for that peer) and
`recv`/`ack_sent` to the opposite one (it generates for me), so a complete
diagnosis needs the logs of both machines:

| reading | where it is broken |
|---------|-----------------|
| `emitted=0` | I am not generating: no rate from the SDN, no peers, or `generator.enabled=false` |
| `emitted>0` here and `recv=0` at the peer | the key is lost in the outbound ORR/QKC — look at `orr.incoming master_secret missing` and `qkc.relay.handle_err` |
| `recv>0` at the peer but its `ack_sent=0` | it did not know where to acknowledge (`ack_no_endpoint`: it holds no ETSI-020 endpoint for me and I announced no legacy `ack_endpoint`), or its POST to me fails (`ack_send_failed`: my 20006 unreachable from it, or its cert is rejected) |
| its `ack_sent>0` and my `acked=0` | the ACK reaches a DKMS that is not me (the endpoint it holds for me points elsewhere: wrong `advertise_ip` announced, or its `peers.<me>.endpoint`), or —legacy socket— 20009 filtered / an `ack_endpoint` that cannot reach me |
| `ack_miss_peer>0` | its `node_id` does not match the `[peers.<id>]` key of my `node.yml` |
| `ack_miss_key>0` | the ACKs arrive late: raise `generator.ack_timeout_ms` |

When `buffer_enc` is at zero a `generator.diag` line is also emitted that
translates the counters into the concrete cause. At boot, with the default
transport, there is no `ack_endpoint` (nothing is announced while the
socket listener is off) and the boot check prints `generator: sin
ack_endpoint anunciado` — harmless, the peers acknowledge over ETSI-020.
With `ack_socket_listen: true` the DKMS validates the `ack_endpoint` it
announces (it warns if it is `0.0.0.0`/loopback and probes it over TCP): if
that check fails, no socket peer will ever be able to acknowledge.

## Multi-institution notes and known limits

- **A single central SDN**: it is the model supported today; per-institution
  federation does not exist yet.
- **QKD links, still manual**: `pqc` links are created by the SDN on the
  fly, but a `qkd` one needs the local `kme_url` in the `node.yml` of both
  ends.
- **A link's rate is shared** between both directions (there is no separate
  A→B / B→A model).
- **No RAM/CPU limits** in the compose files (Docker default). On small
  machines add `mem_limit:` to the service.
- The DKMS key buffers live **in RAM only**: a restart empties them and the
  generator fills them again (by design; there is no persistence of key
  material).
