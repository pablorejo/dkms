# Configuration Helpers (`conf`)

## Purpose

This package provides shared helpers for reading config files and configuring
logging for service processes.

## What it contains

`conf.py` exports helpers such as:

- `CONFIG_FOLDER`
- `TIME_OUT`
- `resolve_rel(path)`
- `get_data_json(type, json_file)`
- `setup_logger(id, type_node)`

## Runtime behavior

- resolve config files from a set of repository-relative candidate paths
- create per-node file loggers under `LOG_DIR`

## Key inputs

- `CONFIG_FOLDER`, defaulting to `code_dkms/config_files`
- `LOG_DIR`, defaulting to `/app/logs`

## Related docs

- [../../config_files/README.md](../../config_files/README.md)
- [../DKMS/README.md](../DKMS/README.md)
- [../ORR/README.md](../ORR/README.md)
- [../QKC/README.md](../QKC/README.md)

## Known constraints

- Config lookup is filename-based, so duplicated filenames across folders can
  become ambiguous.
