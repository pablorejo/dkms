# Domain Models (`models`)

## Purpose

This package defines the shared Pydantic models and enums used across services,
API layers, and persistence mappings.

## What it contains

- `enums.py`: shared enums such as channel and status types
- `model_*.py`: entity-specific models for DKMS, ORR, QKC, SDN, SAE,
  simulations, users, and related records
- `__init__.py`: re-exports and model rebuild wiring

## Runtime behavior

These models are used for:

- config loading and validation
- API request and response contracts
- persistence mapping boundaries

## Key inputs

This package does not read environment variables directly.

## Related docs

- [../persistence/README.md](../persistence/README.md)
- [../db/README.md](../db/README.md)
- [../k8s/README.md](../k8s/README.md)

## Known constraints

- Some model relationships rely on `model_rebuild()` in `models/__init__.py`
  to resolve forward references.
