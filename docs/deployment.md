# Deployment

The maintained path is **multi-host, one institution at a time, with Docker**.
This page is the overview: the model, the order of operations and where each
procedure lives. The step-by-step guide is
[`docker/README.md`](../docker/README.md); the bare commands are in
[`docker/examples/quick_start.md`](../docker/examples/quick_start.md).

## The model

- **One image per module**: `qkc`, `orr`, `dkms`, `sdn`, plus `quditto` for
  QKD links without hardware. Built once per version; institutions only `pull`.
- **One node per institution**: a QKC + ORR + DKMS trio (+ an optional
  quditto), on one machine or spread out. Each module gets a short `node.yml`
  and a compose file copied verbatim from `docker/compose/`; `docker compose
  up -d` is the whole procedure. `site.yml` runs the three on one machine.
- **One SDN for the network**, run by the operator. It is the only shared
  component and it holds no topology file: the graph is built from what the
  modules announce (see [auto-configuration](auto-configuration.md)).
- **No central orchestrator.** Nobody pushes configuration to a node; a node
  registers itself and its peers learn about it on their next heartbeat.

At boot the image's entrypoint (`docker/entrypoint.sh`) renders the mounted
`/config/node.yml` with `docker/render_config.py` into the binary's native
TOML under `/run/cfg/` (a `QUDITTO_*` env file for quditto) and starts the
binary. Field names, default ports and
the TLS blocks are the renderer's job (it mirrors each crate's `src/config.rs`);
the operator edits only `node.yml`. A raw TOML mounted in `/config` is used as is.

## Order of operations

1. **SDN first** — [§ 1. SDN](../docker/README.md#1-sdn-central-operator).
2. **Each node**, in any order — [§ 2. QKC](../docker/README.md#2-qkc),
   [§ 3. ORR](../docker/README.md#3-orr), [§ 4. DKMS](../docker/README.md#4-dkms);
   with `type: qkd` links and no KME hardware, one
   [quditto per link](../docker/README.md#qkd-links-without-hardware-the-quditto-simulator).
3. **Open the ports** listed in
   [Ports: who connects to whom](../docker/README.md#ports-who-connects-to-whom)
   (the per-module bind table is [ipc.md § 4](ipc.md#4-ports)) and read
   [Security and firewall](../docker/README.md#security-and-firewall)
   before exposing anything between institutions.

Boot order is a recommendation, not a requirement. Every module announces
itself to the SDN in a loop (`POST /register/<module>`, which doubles as its
heartbeat) and retries with backoff; an anchor that is not registered yet is
"not yet", not an error, so any order converges
([boot in any order](auto-configuration.md#boot-in-any-order)). The DKMS also
retries the SDN 30×1 s at boot. Warnings during the first minute are the
transient of this dance; judge the steady state.

Adding an institution later touches no running node
([adding a new institution](../docker/README.md#adding-a-new-institution)); when
something does not converge, start at [Troubleshooting](../docker/README.md#troubleshooting).

Each module README has a *Deployment* section with what that module needs and
what its healthy logs look like: [QKC](../qkc/README.md#deployment),
[ORR](../orr/README.md#deployment), [SDN](../sdn/README.md#deployment),
[DKMS](../dkms/README.md#deployment) and [quditto](../quditto/README.md#running-it).

## Local development without containers

```bash
./scripts/build-all.sh          # cargo build --release --workspace

./scripts/run-sdn.sh            # one module per terminal
./scripts/run-qkc.sh
./scripts/run-orr.sh
./scripts/run-dkms.sh
./scripts/run-quditto.sh
```

SDN, ORR and DKMS load their config through `common::config::load_config`:
`config/default.toml` ← `config/local.toml` (optional, gitignored) ←
environment, with the directory taken from `CONFIG_DIR` (the run scripts set
it to `<module>/config`). Environment overrides are `MODULE__section__key`,
e.g. `DKMS__buffer__capacity_per_peer=8192`; overriding one field of a
section merges into it, the other fields of the section survive. Two
exceptions: the QKC takes exactly `qkc --config <path>` (`run-qkc.sh` passes
`qkc/config/default.toml` by default), and quditto reads no file at all — CLI
flags with `QUDITTO_*` environment fallbacks (`--r0`/`QUDITTO_R0`,
`--alpha`, `--distance`, `--listen`, `--tls`, …). Multi-module demos live
in `scripts/demo-3qkc/` and `scripts/demo-star/`.

## Test harnesses

- **Local N-node mesh** — [`tests/local-mesh/`](../tests/local-mesh/README.md):
  SDN + N×(qkc, orr, dkms) on one machine, from the same `node.yml` files and
  the same `render_config.py` the images use (`./mesh.sh up 10`, `keys`,
  `down`). Repeatable in minutes, without hardware.
- **Multi-host testbed** — [`tests/testbed/`](../tests/testbed/README.md) (in
  Spanish): the same against a real compose deployment over SSH, including hot
  add/remove of a node and restart recovery.

## Images

```bash
make images        # local build of the 5 images from docker/Dockerfile
make push          # push with TAG / IMAGE_PREFIX
# or multi-arch (amd64 + arm64), pushed straight to the registry:
IMAGE_PREFIX=youruser docker buildx bake -f docker/docker-bake.hcl --push
```

**Only `docker/Dockerfile` produces deployable images**: it carries
`entrypoint.sh` + `render_config.py` and sets `ROLE`, which is what turns the
mounted `node.yml` into the binary's config. `docker/Dockerfile.workspace`
runs the bare binary, so a compose deployment built from it crash-loops (the
QKC dies at once with `required arguments were not provided: --config`, the
others start without their config). Details of the build in
[Step 0](../docker/README.md#step-0-maintainer-build-and-publish-the-images).

## Certificates

`docker/gen-certs.sh <node_id> <ip> ./certs` issues a node cert (`dkms-N`,
`orr_N`, `qkc-N`, `sdn`; ML-DSA-65 by default) and `--sae <sae_id>` a SAE
client cert. Two roots ([SECURITY.md](SECURITY.md) §2): `net-ca` for nodes
and the control plane, `sae-ca` for SAEs. `net-ca.crt` goes to every
institution; `net-ca.key` never leaves whoever signs. A pre-generated set must
hang off ONE CA — `mesh.sh` checks it (leaf AKI against CA SKI). Which module
needs which files: every role its `<name>.crt`/`.key` + `net-ca.crt`, the
DKMS also `sae-ca.crt` ([Common layout of a
deployment](../docker/README.md#common-layout-of-a-deployment); the three
DKMS planes in [§ 4. DKMS](../docker/README.md#4-dkms)).

## Kubernetes

Not maintained: the `<module>/k8s/` manifests were removed on 2026-08-31
(they predated `node.yml`). If k8s is ever needed again, mount `node.yml`
(ConfigMap) and the certs (Secret) under `/config`, use the current ports
(QKC 20000-20002, ORR 20003-20004, DKMS 20005-20009, SDN 19000/19002/19010,
quditto 20010) and never put a Service on 20007 (`DkmsControl` is loopback).

## Observability

Prometheus `/metrics` on the ORR (20004), the DKMS (20008) and the SDN
(19010) — no auth, answers on any path, internal network only. The QKC has no
Prometheus endpoint: its state is on the admin HTTP (`:20002/stats`). quditto
serves `/healthz` on its HTTP port (20010). The DKMS state is read from its
`generator.state` log line (one per peer, every 5 s).

## Saturation / load tests — run isolated

Local saturation runs have OOM-killed a desktop session. Run them inside a
memory-bounded cgroup (`systemd-run --user --scope -p MemoryMax=8G …`, or Docker
with `--memory`), keep artifacts under `tests/results/` (gitignored, never
`/tmp`), and read the rule first: [engineering notes](engineering-notes.md#saturation--load-tests--run-isolated).
