# Default Configuration Files

## Purpose

This directory contains the baseline JSON templates used by local runs and by
some topology-generation and deployment flows.

## What it contains

- [DKMS/README.md](DKMS/README.md): DKMS node templates
- [ORR/README.md](ORR/README.md): ORR node templates
- [QKC/README.md](QKC/README.md): QKC node templates
- [SDN/README.md](SDN/README.md): SDN template
- [AgentControllers/README.md](AgentControllers/README.md): agent-controller
  templates

## Runtime behavior

- Services can load these files through helpers in `src/conf/conf.py`.
- `scripts/create_topology_json.py` and related tooling can regenerate this
  tree from a simpler topology description.

## Related docs

- [../../docs/TOPOLOGY_SCHEMA.md](../../docs/TOPOLOGY_SCHEMA.md)
- [../../scripts/README.md](../../scripts/README.md)
- [../README.md](../README.md)

## Known constraints

- IDs and cross-references across subfolders must stay consistent.
- Generated content may overwrite manual edits during topology regeneration.
