# QKC — the key-material layer of a node

The QKC (Quantum Key Channel) is the module of a node that faces the other
nodes' QKCs. It keeps one link per neighbour, `qkd` (link keys from an ETSI
GS QKD 014 KME) or `pqc` (link keys derived from ML-KEM epochs), and relays
frames hop by hop: each hop is encrypted with a link key (a one-time pad)
from that link's buffer, and the next hop comes from the forwarding table
the SDN pushes.
What flows through it is the DKMS's transport material and the ORR's onion
frames. It never sees a transport key: between the decrypt of one hop and
the re-encrypt of the next it holds exactly what the ORR handed over, the
DKMS's e2e-sealed `DKMS_BUFFER`, and it never parses the ORR or DKMS
headers it carries.

## What it does

- **Keeps one link per neighbour QKC**, a `LinkRuntime` per `[[links]]`
  entry ([`src/service.rs`](src/service.rs)): key source, ENC/DEC keystore,
  handshake, frame authenticator. The SDN can add and remove `pqc` links
  at runtime.
- **Encrypts every hop with OTP**, one key of `key_size_bits` per block
  ([`src/crypto.rs`](src/crypto.rs)), and **fills the link buffers in the
  background** ([`src/keystore.rs`](src/keystore.rs)): an ENC worker takes
  keys from the source and announces their ids with `FRAME_KEY_IDS_NOTIFY`,
  a DEC worker fetches what the neighbour announced.
- **Authenticates the link twice**: the handshake with the node certificate
  ([`src/pqc_handshake.rs`](src/pqc_handshake.rs)) and every frame with a
  link MAC plus anti-replay window ([`src/frame_auth.rs`](src/frame_auth.rs)).
- **Forwards by a WCMP table** the SDN pushes to `POST /forwarding-table`
  ([`src/routing.rs`](src/routing.rs), [`src/relay.rs`](src/relay.rs)), and
  **measures each `qkd` link's rate in situ** for the SDN to size the edge
  ([`src/rate_estimator.rs`](src/rate_estimator.rs)).
- **Announces itself** every `sdn_announce_secs` (30 s) with
  `POST /register/qkc`, also its heartbeat ([`src/sdn_client.rs`](src/sdn_client.rs)).
- **Serves the co-located ORR** on the local port ([`src/transport/local.rs`](src/transport/local.rs))
  and **logs its state every 5 s** (`keystore.levels`, `qkc.links`, `qkc.frame_auth`).

## How it works

End-to-end flows are in [architecture.md](../docs/architecture.md); this is
the QKC's part of them.

### Frames and listeners

A `wire::Frame` ([`wire/src/lib.rs`](../wire/src/lib.rs), [ipc.md](../docs/ipc.md))
carries kind, `grade`, `sender_id`, `receiver_id`, `dest_final`,
`key_size_bits`, `epoch_id`, the `key_ids` of the payload, two cleartext
headers (`header_orr_mp`, `header_dkms_mp`) and the payload. The QKC reads
`sender_id` (which link), `dest_final`, `grade` and `key_ids`, and rewrites
the per-hop fields (`sender_id`, `receiver_id`, `key_size_bits`, `key_ids`,
payload); `dest_final`, `grade`, `epoch_id` and the two headers are copied
unchanged.

| Listener | Config (default) | Frames | From |
|---|---|---|---|
| peer | `peer_listen` (`0.0.0.0:20000`) | `FRAME_RECV`/`FRAME_RELAY` (0x01/0x02; MAC'd 0x04/0x05), NOTIFY (0x20; 0x25), handshake (0x21–0x24, 0x26–0x27), resync (0x28–0x2A) | neighbour QKCs |
| local | `local_listen` (`127.0.0.1:20001`) | `FRAME_LOCAL_SEND` in, `FRAME_LOCAL_DELIVER` out, plaintext | this node's ORR |
| admin | `admin_http` (`0.0.0.0:20002`) | `POST`/`GET /forwarding-table`, `GET /healthz`, `GET /stats` | the SDN; the operator |

### One hop, end to end

1. The ORR writes a `FRAME_LOCAL_SEND` with `dest_final` and a 32-byte
   plaintext (one sealed transport key).
2. The table gives the next hop, keyed by a hash of `dest_final` and the
   headers so a flow keeps its path; a direct neighbour is its own next hop.
3. The outgoing link's ENC buffer gives one key per `key_size_bits / 8`
   bytes; the payload is XORed, the `key_ids` go in the frame. A short
   buffer makes the relay wait for the worker (up to 30 s), never call the
   KME. More than 4 blocks is refused without spending keys.
4. `frame_auth` seals the frame; it leaves as `FRAME_RECV` (last hop) or
   `FRAME_RELAY` over the persistent connection to that neighbour
   ([`src/transport/peer_client.rs`](src/transport/peer_client.rs)). There,
   the connection reader verifies MAC and window in arrival order and
   queues it (intake 8 192; overflow counts `intake_dropped_full`) for at
   most 8 192 concurrent tasks.
5. Each `key_id` is taken from the incoming link's DEC buffer, waiting up to
   2 s if the NOTIFY outran the keys (an id no NOTIFY announced fails at
   once). A link key is consumed on lookup, so a timed-out frame is lost; the
   DKMS expires the key on ACK timeout (30 s) and emits fresh material.
6. `dest_final == qkc_id`: `FRAME_LOCAL_DELIVER` to the ORR. Otherwise
   steps 2 to 4 on the next link. A `sender_id` with no link here is
   dropped (`UnknownNeighbor`).

One key per block is why payload bytes are expensive on a `qkd` link (with
256-bit keys, 33 bytes cost two KME keys per hop): tags, epochs and
counters ride in the headers, never in the payload
([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas),
"every byte added to the payload costs QKD key material").

### QKD links: the KME and the two buffers

A `qkd` link takes its link keys from a KME over ETSI-014 ([`src/kme.rs`](src/kme.rs)):
`quditto_url` in the TOML, `kme_url` in `node.yml`. Both ends talk to the
same KME pair (one per end on hardware, one `quditto` in simulation). The
only KME parameter to declare is `sae_id` (default `qkc_id`, what quditto
expects); key size and `max_key_per_request` are read from `/status` at the
first refill, the KME's key size overriding the config with a warning.

| Piece | What it does |
|---|---|
| ENC ring | up to 4 096 keys from `enc_keys`; the relay pops from it |
| ENC worker | below 256 keys it fetches batches of 128 and sends each batch's ids in a NOTIFY |
| DEC map | `key_id → link key` for what the neighbour announced |
| DEC worker | fetches announced ids with `dec_keys`, 128 per call (one per call once a KME rejects the batch) |
| stock poller | `GET /status` every second (every 250 ms until the first estimate) for the estimator; from ¾ of the KME's `max_key_count` it *banks* batches into the ring so material the KME would discard at its ceiling stays usable |

An `https://` KME (the renderer's default for a bare `kme_url`) is dialled
with mTLS: the per-KME triple `kme_cert` / `kme_key` / `kme_ca` (all three
or none; each KME has its own PKI), else the node certificate, which a
quditto issued from the network CA accepts. An `http://` KME is plaintext:
whoever answers is trusted to be the QKD device.

### PQC links: epochs from ML-KEM

The QKC with the numerically lower `qkc_id` is the initiator. Per epoch it sends
`FRAME_PQC_KEM_INIT` (its `_AUTH` / `_SIGNED` variant under `pqc_auth`)
with a fresh ML-KEM public key (`pqc_suite`, `ml-kem-768`); the responder
encapsulates, answers `FRAME_PQC_KEM_RESP`, and both hold a 32-byte
secret. Link keys are derived locally as
`HKDF-SHA256(secret[epoch], key_id)` ([`src/pqc_source.rs`](src/pqc_source.rs));
keystore, NOTIFY and relay are as on a `qkd` link. The epoch is the first
four bytes of every `key_id`, so nothing has to stay in step between the
ends. A new epoch opens every `pqc_rekey_keys` keys (1 000) or
`pqc_rekey_secs` seconds (3 600), whichever first, `pqc_rekey_lookahead`
(2) epochs ahead; old epochs are zeroised. Logs:
`qkc.pqc.handshake.established` per epoch, `qkc.pqc.rotation`.

A restart leaves the ends with different epoch windows, which the OTP alone
never shows. **Relink on reconnect**: a TCP reconnect (never the first
connect) makes the initiator negotiate a fresh block above the current
highest and prune below it, at most once per 5 s; the responder, which may
not send INIT, re-encapsulates an INIT that carries a new public key.
**Resync on an unknown epoch**: the DEC side reads the peer's epoch off
every `key_id` and MAC trailer; the initiator relinks above *both* windows,
the responder asks with `FRAME_PQC_RESYNC_REQ` (0x28). Recovery is
reactive: an idle broken link heals at the first frame. Each rule was the
fix of a measured failure
([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas),
"A restarted QKC restarts its epoch numbering", "PQC link recovery").

### Authenticating the link: handshake and per-frame MAC

OTP gives confidentiality and nothing else: XOR is malleable, `sender_id`
is unchecked, a captured frame replays. Two layers close that, both per
link and both *on* by default when the node has a certificate (`[tls]`,
written by the renderer unless `control_tls: false`):

| Field | Values | Default with `[tls]` / without | Protects |
|---|---|---|---|
| `pqc_auth` | `off`, `prefer`, `require` (HMAC with `link_psk`), `sign` (ML-DSA-65) | `sign` / `off` | the handshake: whom you agree an epoch with |
| `frame_auth` | `off`, `prefer`, `require` | `require` / `off` | each data frame and NOTIFY: integrity, origin, freshness |

With `sign`, INIT and RESP carry the node certificate chain and a signature
by its key, verified against the network CA and the SAN `dkms://qkc-<id>`,
so a link the SDN creates is authenticated with nothing distributed per
pair; on a `qkd` link the handshake only seeds the MAC root, the link keys
still come from the KME. The MAC is an HMAC-SHA256 over the whole frame in a
48-byte trailer `session ‖ counter ‖ tag`, rooted in a `link_psk` (32
bytes, base64, identical at both ends, [rollout](../docker/README.md#authenticating-the-link-link_psk--frame_auth);
`session` is then a random incarnation per process run)
or, by default, in the secret of the link's current epoch (`session` = the
epoch). A NOTIFY is sealed whenever a root exists, whatever `frame_auth`
says: it decides which `key_ID`s the neighbour asks its KME for, and QKD
does not protect that. The receiver keeps a 1 024-counter window per
session, verified after the MAC; a frame sealed with an epoch this side
lacks is dropped (counted in `bad_mac`) and requests a resync, unless a
frame from that peer was verified in the last 10 s (a restarted peer sends
none; a forger coexists with them). `require` without a root refuses to
start. Both checks run in the connection reader, in arrival order: in the
concurrent dispatcher tasks they rejected legitimate frames as replays
under load ([engineering
notes](../docs/engineering-notes.md#active-known-issues--gotchas), "The link
MAC and the anti-replay window are checked in the connection READER").

### Forwarding: the WCMP table

No route is configured. [`src/routing.rs`](src/routing.rs) holds
`dest_qkc → [(next_hop, weight)]` in an `ArcSwap`; the SDN replaces it with
`POST /forwarding-table {"replace": …}` on every push. Direct neighbours
short-circuit it; among several next hops the choice is deterministic in
the frame hash, so each QKC decides alone and a flow keeps its path.
`replace_qkd` sets a QKD-only table: a frame whose `grade` byte says QKD
(0; PQC is 1, chosen by the DKMS and written into the frame by the ORR)
uses only that table and direct `qkd` neighbours, so a key promised as QKD
never crosses a `pqc` link (until a QKD table is pushed, which the SDN does
only with `SDN_DUAL_GRADE_TABLES=1`, such frames use the full table).
Tables follow topology and capacity, never buffer levels
([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas),
"Routing is decoupled from the rate solver"). Under mTLS the push is
accepted only from the certificate with SAN `dkms://sdn`.

### The in-situ rate estimator

`r0` / `alpha` / `distance_km` are the SDN's cold-start prior for a `qkd`
edge; a real deployment does not know them. So the QKC measures each `qkd`
link from what every KME exposes, `stored_key_count` in `/status`, plus the
drains it already counts: `produced = ΔS + drained`, the neighbour's drains
counted when its NOTIFY arrives; intervals in which the stock touched
`max_key_count` are censored (the KME discarded or paused). The estimate is
a time-weighted mean over a 30 s horizon. Each announcement carries
`measured_rate_keys_per_s`, `measured_age_ms` and `measured_quality`:
`measured`, `floor` (no valid window for 15 s, typically full and idle: the
last value holds as a lower bound and never decays to 0, which would starve
the link) or `unavailable` (five `/status` failures in a row). The SDN takes
the minimum of the two ends. Design and measurements:
[QKD rate estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01).

### The announce loop and the peer set

With `sdn_url`, [`src/sdn_client.rs`](src/sdn_client.rs) announces every
`sdn_announce_secs`; while something has not converged (SDN unreachable,
`edges_pending` non-empty) it retries every 2 s, doubling up to the period,
and logs `anunciado a la SDN` only when the outcome changes. An edge exists
once both ends are registered, so any boot order converges. The response's
`peers` is what makes hot growth possible: a `pqc` peer not yet linked is
created with the `peer_addr` the SDN gives, using this node's `[[links]]`
config for that neighbour if there is one (the SDN only contributes the
address), else the settings of the first declared link (or the built-in
`pqc` defaults; `key_size_bits` is the edge's, 256 unless declared) and no
per-pair secret, at most 64 such links. A `qkd` peer with
no local link is ignored with a warning: it needs this institution's
`kme_url`, which the SDN cannot know. A link the SDN added and no longer
lists is torn down and zeroised; a link from `node.yml` is never removed,
because an SDN briefly behind would otherwise destroy live links
([peers ride back on the announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement)).

### When the neighbour restarts

- **`pqc` link**: the reconnect (or the first unknown epoch) triggers the
  relink or resync above; expect `qkc.pqc.relink: renegocio el enlace` on
  the initiator, then `qkc.pqc.handshake.established` on both ends.
- **`qkd` link**: the restarted end boots with empty buffers; frames using
  link keys it never saw count `misses` (and `incoming_errs` in `/stats`)
  and are lost until this side's ENC ring has drained them (up to 4 096);
  the DKMS expires those keys on ACK timeout and emits fresh material. With
  the link MAC rooted in the epoch secret, the handshake relinks as on a
  `pqc` link.
- **This QKC restarts**: `node.yml` links with an address come up at once;
  those declared by id alone and the SDN-created ones return with the next
  announcement, the forwarding table with the SDN's next push (it follows
  every rate recompute, `mcf_period_ms`, 5 s).
- **The SDN restarts**: links and tables stay; the loop repopulates it.

## Deployment

### Minimal `node.yml`

From [`docker/examples/node.qkc.yml`](../docker/examples/node.qkc.yml); the
container's [`render_config.py`](../docker/render_config.py) turns it into
`qkc.toml` (fields in [`src/config.rs`](src/config.rs)).

```yaml
qkc_id: 1                          # required; numeric. Cert name: qkc-1
sdn_url: "10.0.0.100"              # SDN HTTP admin; port 19002 and https:// added (control_tls)
advertise_ip: "10.0.0.11"          # the IP the SDN and neighbours reach; without it the QKC
                                   # runs but does not announce (listeners bind 0.0.0.0)
links:
  - neighbor_id: 2                 # required; the only topology a QKC declares
    type: pqc                      # pqc (default) | qkd
    # neighbor_addr: "10.0.0.12"   # optional on pqc with sdn_url (the SDN supplies it);
                                   # required on qkd. Port 20000 added if absent
  # - neighbor_id: 3
  #   type: qkd
  #   neighbor_addr: "10.0.0.13"
  #   kme_url: "https://my-kme:443"        # required on qkd; https:// added if bare
  #   r0: 2000                             # optional prior for the SDN (keys/s at 0 km)
  #   alpha: 0.2                           # dB/km
  #   distance_km: 5

# key_size_bits: 256               # OTP block, all links; must match the neighbour
# control_tls: true                # false = plaintext announce/admin, on ALL nodes at once
# certs_dir: /config/certs         # where qkc-<id>.crt/.key and net-ca.crt are mounted
```

Also optional: `sdn_announce_secs` (30), `cert_name` (`qkc-<id>`),
`listen_ip`, `local_bind` (the ORR port, `127.0.0.1`), `ports`,
`sign_secret_seed`, `extra` (any other top-level `src/config.rs` key); per
link, `kme_cert` / `kme_key` / `kme_ca` (paths as mounted),
`capacity_keys_per_s` (`pqc`; SDN default 10 000), `pqc_suite`,
`pqc_rekey_*`, `link_psk`, `frame_auth`, `pqc_auth`, `peer_verify_key`. The
renderer refuses a `qkd` link without `neighbor_addr` or `kme_url`, and a
`pqc` link without `neighbor_addr` when there is no `sdn_url`. `sae_id` has
no `node.yml` key today; a raw `qkc.toml` mounted in `/config` bypasses the
renderer ([`scripts/demo-idq/`](../scripts/demo-idq/README.md) runs two
such TOMLs directly). Mind that the binary's own default for a `[[links]]`
entry that omits `key_size_bits` is 1024, not the 256 the renderer and the
SDN assume: write it explicitly in a raw TOML.

### Ports

| Port | Config | Protocol | Who connects | TLS |
|---|---|---|---|---|
| 20000 | `peer_listen` | binary TCP wire | the neighbour QKCs, both directions | none; the signed handshake and the link MAC authenticate frames |
| 20001 | `local_listen` | binary TCP wire, plaintext | this node's ORR | none, no authentication: loopback by default |
| 20002 | `admin_http` | HTTP | the SDN (forwarding push); the operator (`/healthz`, `/stats`) | mTLS with `[tls]`, client cert from the network CA required; plaintext with `control_tls: false` |

Outbound: every neighbour's 20000, the SDN admin (19002, `https://` by
default) and, per `qkd` link, the KME (quditto listens on 20010). Firewall
view: [Ports: who connects to whom](../docker/README.md#ports-who-connects-to-whom).

### Certificates

| File | Made by | What it authenticates |
|---|---|---|
| `qkc-<id>.crt` / `.key` in `certs_dir` | `docker/gen-certs.sh qkc-<id> <advertise_ip> ./certs` (ML-DSA-65, SAN `dkms://qkc-<id>`) | this QKC to the SDN admin (which binds the announced `id` to the SAN), as server on 20002, as signer of the link handshake, and to an https KME that trusts the network CA |
| `net-ca.crt` | shared across the federation | the SDN's certificate on the push, the neighbours' handshake chains, the SDN as server |
| `kme_cert` / `kme_key` / `kme_ca` per `qkd` link | the KME operator's PKI | this QKC as ETSI-014 client of that KME, and that KME as server |

TLS negotiates only the hybrid `X25519MLKEM768`; the binary self-checks a
handshake with its own identity at boot and aborts otherwise.

### What it announces, what comes back

`POST <sdn_url>/register/qkc` with `id`, `host` (`advertise_ip` + admin
port), `peer_addr` (`ip:20000`, what neighbours dial) and `links[]`:
`neighbor_id`, `link_type`, the optional `r0_keys_per_second` / `alpha` /
`distance_km` / `pqc_capacity_keys_per_s` and, on `qkd` links, the measured
rate, quality and age. Neighbour addresses, secrets and authentication
settings never travel; under mTLS the SDN accepts the body only if the
certificate's SAN is `dkms://qkc-<id>`. The response carries `changed`,
`edges_added`, `edges_pending` (neighbour not registered yet), `edges_removed`
(this announcement stopped declaring the neighbour and the other end does not
declare it either), `edges_conflict` (the
neighbour declared different link parameters; the SDN keeps the first and
this QKC warns once) and `peers`: `[{qkc_id, peer_addr, link_type,
key_size_bits}]`, sorted by id.

### Running without containers

```bash
./scripts/run-qkc.sh                 # cargo run --release -p qkc -- --config qkc/config/default.toml
./target/release/qkc --config /path/to/qkc.toml
```

The QKC is the exception to the `CONFIG_DIR` / environment scheme of the
other modules: it loads exactly the TOML given with `--config` and reads no
`QKC__*` variables. [`config/default.toml`](config/default.toml) is the
development config (ports 20000–20002, no links, no `[tls]`); `qkc-test-client`
(`send`, `listen`, `stress`) stands in for an ORR, and [`scripts/demo-3qkc/`](../scripts/demo-3qkc/)
and [`scripts/demo-star/`](../scripts/demo-star/) run several QKCs. Full
procedure: [docker/README.md § 2. QKC](../docker/README.md#2-qkc), the
[quick start](../docker/examples/quick_start.md#2-qkc) and, without QKD
hardware, [the quditto simulator](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator).

## Health and diagnostics

At boot: `qkc starting` with the config (secrets redacted), the three
`*.listening` lines, `qkc.frame_auth.enabled` per authenticated link and,
per neighbour, `qkc.peer_out.connected` and `qkc.pqc.handshake.established`.
Every 5 s, one `keystore.levels` line per live link:

| Field | Meaning |
|---|---|
| `peer` | neighbour id |
| `rate`, `rate_q` | estimated rate (keys/s) and its quality; absent on `pqc` links |
| `enc`, `dec` | keys in the ENC ring (to encrypt towards the peer) and in the DEC map (announced by the peer) |
| `taken` | keys popped from ENC by the relay since boot |
| `misses` | DEC lookups that found no key; brief after a NOTIFY, steady growth is a divergence |
| `wenc`, `wenc_to` | relay waits for ENC keys, and timeouts (30 s) |
| `wdec`, `wdec_to` | relay waits for a DEC key, and timeouts (2 s): each one is a lost frame |
| `enc_drop` | source keys that did not fit the ring (paid material lost; stays 0) |
| `refill_fail` | failed `enc_keys` calls (KME down, TLS, timeout, no epoch yet) |
| `notify_drop` | NOTIFYs not sent (outbound queue full) or ids discarded (DEC pending list full) |

Alongside: `qkc.links` (`live`, `declared`, `waiting` = neighbours declared
by id whose address the SDN has not supplied yet), `qkc.frame_auth` per
authenticated link (`mode`, `session`, `signed`, `verified`, `bad_mac`,
`replayed`, `plain_ok`, `plain_rej`), every 30 s `qkc.peer_out.stats`
(`connected`, `queue_depth`, `sent_total`, `dropped_total`), and `GET /stats`
with the per-link levels and waits under their long names (without
`enc_drop` / `refill_fail` / `notify_drop`), `peer_out_sent` /
`peer_out_dropped` per peer and the service counters (`incoming_*`,
`deliver_*`, `local_send_*`, `intake_dropped_full`). Healthy: `enc` and
`dec` non-zero, `taken` growing with traffic, `misses`, `wdec_to`,
`enc_drop`, `refill_fail`, `dropped_total` and `intake_dropped_full` flat,
`bad_mac = replayed = plain_rej = 0`, `plain_ok = 0` under `require`,
`waiting` empty.

Symptoms an operator will meet:

- **`enc=0 dec=0 taken=0` on a `pqc` link with a healthy
  `handshake.established` history at both ends**, plus `refill_fail`
  climbing by one every ~10 s with `keystore.enc_refill failed … timeout
  waiting pqc-secret` (the warning itself is throttled to powers of two):
  different epoch windows and no relink running
  ([engineering notes](../docs/engineering-notes.md#active-known-issues--gotchas),
  "PQC link recovery after a single-end restart").
- **The peer's DKMS shows `recv=0` while this side's DKMS `expired`
  climbs**, with normal-looking `keystore.levels`: frames die undecrypted.
  Look at the neighbour's `misses` / `wdec_to` and for `qkc.pqc.relink` on
  the initiator ("A restarted QKC restarts its epoch numbering", same page).
- **`qkc.notify: unknown neighbor`, or `qkc.relay.handle_err` with
  `no link/quditto for neighbor N`**: the neighbour declares a link this
  node does not (a `qkd` link must be declared at both ends), or the SDN
  has not yet delivered the peer set (`waiting` non-empty). The related
  **`la SDN anuncia un enlace QKD que no tengo configurado`** means the
  `qkd` link must be added to this `node.yml` with its `kme_url`.
- **`intake_dropped_full` climbing on a transit QKC** (the star hub):
  more frames arrive than the in-flight tasks absorb and the senders'
  DKMSs expire keys; structural at high fan-in ([campaign 2026-09, the star
  hub](../docs/results/campaign-2026-09.md#72-the-star-hub-intake-queue-and-expired-keys-structural-limit)).
  **`replayed` climbing with `bad_mac = 0`** is the arrival-order invariant
  broken ([campaign 2026-09, link MAC](../docs/results/campaign-2026-09.md#71-link-mac-legitimate-frames-rejected-as-replays-fixed)).
- **`plain_rej` climbing under `require`, or `plain_ok` not reaching 0
  under `prefer`**: the other end is not sealing. **`qkc.http_admin push
  rechazado`**: the pusher's certificate is not `sdn`. **`qkc.http_admin
  tls handshake failed` on every push**: `control_tls` differs between SDN
  and QKC.

## Where things live

| File | Responsibility |
|---|---|
| [`src/main.rs`](src/main.rs), [`src/config.rs`](src/config.rs) | `--config`, hardening, TLS self-check, listeners, announcer; `QkcConfig`, `LinkConfig`, defaults, validation |
| [`src/service.rs`](src/service.rs) | `QkcService`: links in `ArcSwap`, `LinkRuntime` build, hot add/remove, the 5 s state lines |
| [`src/relay.rs`](src/relay.rs), [`src/crypto.rs`](src/crypto.rs) | the hop: decrypt on the incoming link, deliver or re-encrypt, header and `epoch_id` propagation; OTP over blocks |
| [`src/keystore.rs`](src/keystore.rs), [`src/kme.rs`](src/kme.rs) | ENC ring, DEC map, refill workers, NOTIFY, banking, `keystore.levels`; `KeySource` and the ETSI-014 client |
| [`src/pqc_source.rs`](src/pqc_source.rs), [`src/pqc_handshake.rs`](src/pqc_handshake.rs) | epoch secrets and HKDF derivation; ML-KEM INIT/RESP per epoch, rotation loop, relink, resync |
| [`src/frame_auth.rs`](src/frame_auth.rs) | HMAC trailer, PSK or per-epoch root, anti-replay window, `qkc.frame_auth` counters |
| [`src/rate_estimator.rs`](src/rate_estimator.rs) | conservation-with-censoring estimate and its quality |
| [`src/routing.rs`](src/routing.rs) | WCMP `ForwardingTable`, full and QKD-only, flow-affine choice |
| [`src/http_admin.rs`](src/http_admin.rs), [`src/mtls_admin.rs`](src/mtls_admin.rs) | admin routes; the mTLS listener binding the push to the `sdn` identity |
| [`src/sdn_client.rs`](src/sdn_client.rs) | announce loop, peer-set application, `node.yml`-as-floor rule |
| [`src/transport/`](src/transport/) | `peer_server.rs` (reader-side authentication, bounded intake), `peer_client.rs` (persistent connections, reconnect signal), `local.rs` (the ORR port) |

The rustdoc has the rest: `make doc-open`.

## Further reading

- [Architecture](../docs/architecture.md), [auto-configuration](../docs/auto-configuration.md), [IPC](../docs/ipc.md) and [`wire/README.md`](../wire/README.md).
- [docker/README.md § 2. QKC](../docker/README.md#2-qkc), [authenticating the link](../docker/README.md#authenticating-the-link-link_psk--frame_auth), [ports](../docker/README.md#ports-who-connects-to-whom), [security and firewall](../docker/README.md#security-and-firewall), [the quditto simulator](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator) and [`quditto/README.md`](../quditto/README.md); [deployment.md](../docs/deployment.md).
- Engineering notes: [topology is inferred](../docs/engineering-notes.md#topology-is-inferred-never-configured), [peers ride back on the announcement](../docs/engineering-notes.md#peers-ride-back-on-the-announcement), [theoretical rate — not R0](../docs/engineering-notes.md#theoretical-rate--not-r0), [QKD rate estimation](../docs/engineering-notes.md#qkd-rate-estimation-in-situ-2026-09-01), [gotchas](../docs/engineering-notes.md#active-known-issues--gotchas) (epoch restart and resync, link MAC in the reader, PQC recovery on reconnect, OTP cost per block, `epoch_id` propagation, routing decoupled from rates).
- [SECURITY.md](../docs/SECURITY.md): [Fase 5](../docs/SECURITY.md#fase-5--qkcqkc-handshake-pqc-autenticado-hmac-psk) (authenticated handshake), [Fase 8](../docs/SECURITY.md#fase-8--integridad-origen-de-datos-y-frescura-del-plano-de-datos-hecha-2026-08-28) (per-frame integrity and freshness), [Fase 10](../docs/SECURITY.md#fase-10--dos-raíces-de-confianza-por-salto-cert-de-nodo-en-el-qkc-autorización-de-superficies-hecha-2026-08-31) (two trust roots per hop: node certificate and KME).
- [Campaign 2026-09](../docs/results/campaign-2026-09.md): the two QKC findings, [link MAC replay rejection](../docs/results/campaign-2026-09.md#71-link-mac-legitimate-frames-rejected-as-replays-fixed) (fixed) and the [star hub's intake limit](../docs/results/campaign-2026-09.md#72-the-star-hub-intake-queue-and-expired-keys-structural-limit) (structural).
- Harnesses: [local mesh](../tests/local-mesh/README.md), [testbed](../tests/testbed/README.md) (T40–T43: PQC link restarts and reconnects), [`scripts/test-rate-estimator.sh`](../scripts/test-rate-estimator.sh), [`scripts/demo-idq/`](../scripts/demo-idq/README.md) (the QKC over real ID Quantique KMEs).
- Neighbours: [ORR](../orr/README.md), [DKMS](../dkms/README.md), [SDN](../sdn/README.md). [`proto/qkc.proto`](../proto/qkc.proto) defines a `QkcControl` gRPC service the binary does not serve: the QKC has no gRPC listener; its control plane is the HTTP admin and the announcement.
