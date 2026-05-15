# Persistence Abstraction

## Purpose

This package provides the repository and unit-of-work abstraction used by newer
service code.

## What it contains

- `ports.py`: repository and unit-of-work interfaces
- `composition.py`: backend selection from environment
- `memory/`: in-memory implementation
- `sqlalchemy/`: SQLAlchemy implementation, mappers, repositories, UoW, ORM
  entities (`data.py`), and SQL migrations (`migrations/`)

Key entry point:

- `build_uow_from_env` in [`composition.py`](composition.py)

## Runtime behavior

Backend selection is environment-driven:

- `PERSISTENCE_BACKEND=memory` uses the in-memory implementation
- `PERSISTENCE_BACKEND=sqlalchemy` uses the SQLAlchemy implementation and
  requires `DB_URL`

## Key inputs

- `PERSISTENCE_BACKEND`
- `DB_URL`

## Related docs

- [../authz/README.md](../authz/README.md)
- [../k8s/README.md](../k8s/README.md)
- [../models/README.md](../models/README.md)

## Known constraints

- The SQLAlchemy backend requires complete mapper coverage for persisted
  entities.
