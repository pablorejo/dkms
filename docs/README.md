# Documentation map

One line per document, grouped by what you are trying to do. The code is
the reference for anything a document does not say; every claim below was
checked against it when written, and `make linkcheck` keeps the links alive.

Unless noted, everything is in English. [SECURITY.md](SECURITY.md) and
[tests/testbed/README.md](../tests/testbed/README.md) are still in Spanish.

## I want to run a node

| Read | What it answers |
|---|---|
| [deployment.md](deployment.md) | The deployment model in ten minutes: one image per module, one `node.yml` per institution, what to start first, where each detail lives. |
| [docker/examples/quick_start.md](../docker/examples/quick_start.md) | Just the commands, in order, for the SDN and for one node. |
| [docker/README.md](../docker/README.md) | The full procedure: ports and firewall, certificates, the `node.yml` of each module field by field, day-to-day operation, adding an institution, troubleshooting. |
| [qkc/README.md § Deployment](../qkc/README.md#deployment), [orr/README.md § Deployment](../orr/README.md#deployment), [dkms/README.md § Deployment](../dkms/README.md#deployment), [sdn/README.md § Deployment](../sdn/README.md#deployment) | The minimal `node.yml`, ports, certificates and health line of one module. |
| [quditto/README.md](../quditto/README.md) | Running QKD links without hardware: the simulated KME and how a QKC points at it. |
| [tests/local-mesh/README.md](../tests/local-mesh/README.md) | An N-node network on one machine from the same `node.yml` renderer, with an end-to-end key check. |

## I want to understand the design

| Read | What it answers |
|---|---|
| [architecture.md](architecture.md) | What a node is, the two kinds of keys, how a key travels end to end, which layer protects what against whom, what survives a restart. Start here. |
| [auto-configuration.md](auto-configuration.md) | How the topology builds itself from announcements, what the SDN sends back, what happens when a node is added, removed, rewired or restarted, and what is not automatic. |
| [ipc.md](ipc.md) | The three planes as they really are: every gRPC service and RPC, every HTTP route and who may call it, the binary wire and its frame kinds, the ports. |
| [qkc/README.md](../qkc/README.md), [orr/README.md](../orr/README.md), [dkms/README.md](../dkms/README.md), [sdn/README.md](../sdn/README.md) | Each module: what it does, how it works at the architecture level, how it is deployed, how you know it is healthy, where the code lives. |
| [wire/README.md](../wire/README.md), [etsi/README.md](../etsi/README.md), [common/README.md](../common/README.md) | The three libraries: the frame format, the ETSI GS QKD 014/020 models, the shared plumbing and crypto. |
| [SECURITY.md](SECURITY.md) (Spanish) | The threat model, the two certificate authorities, the hardening phases and the decisions behind them. |
| [engineering-notes.md](engineering-notes.md) | The invariants, the defaults and the gotchas measured along the way, with dates. Read it before touching the topology, the rate path or the key-material path. |

## I want to see it measured

| Read | What it answers |
|---|---|
| [results/campaign-2026-09.md](results/campaign-2026-09.md) | The CESGA campaign: six topologies, N = 10 to 100, three loads, with every security default on. Throughput against the fibre ceiling, fairness, latency, the two code findings, the harness limits. |
| [tests/local-mesh/README.md § Measuring](../tests/local-mesh/README.md#measuring) | The before/after measurements of multipath and of the emit loop on the local mesh. |
| [tests/testbed/README.md](../tests/testbed/README.md) (Spanish) | The multi-host test plan on the Proxmox testbed: functional, load, hot add and removal of a node, PQC link recovery, integrity. |
| [engineering-notes.md § Roadmap and measured state](engineering-notes.md#roadmap-and-measured-state) | What has been measured where, and what remains. |

## I want to change the code

| Read | What it answers |
|---|---|
| `make doc-open` | The rustdoc of the whole workspace, private items included, with a landing page per crate. Each crate root opens with what the module is and how it talks to the others. |
| [engineering-notes.md](engineering-notes.md) | The invariants that must not be broken and the measured reason for each. |
| `make check` | The gate CI runs: format, clippy, rustdoc, Markdown links, the `node.yml` renderer tests, and every test with no skips. See [../README.md](../README.md#building). |
| [../proto/](../proto/) | The protobuf schemas, which are the boundary between modules. |
| The `Where things live` section of each module README | Which source file owns which responsibility. |

## Conventions

- **Terms** are defined once, in the [glossary of architecture.md](architecture.md#10-glossary): node, SDN, SAE, KME, link, transport key, session key, announcement, peer, forwarding table, epoch, link MAC, onion, e2e seal, grade, incarnation.
- **Links are relative** and checked by `scripts/check-md-links.py` (`make linkcheck`), fragments included: renaming a heading that another document links to fails the gate.
- **Dates** mark when something was measured or decided, not when the document was written.
- Local, unversioned notes (audits, work logs) are not part of this set; what they established is in `engineering-notes.md`.
