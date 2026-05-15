from __future__ import annotations

from typing import Optional

from .repos import (
    AgentControllerRepoMemory,
    DKMSRepoMemory,
    ORRRepoMemory,
    QKCRepoMemory,
    SAERepoMemory,
    SimulationRepoMemory,
    UserRepoMemory,
)
from ..ports import Repositories


def _default_store() -> dict:
    return {
        "counters": {
            "sim": 0,
            "dkms": 0,
            "user": 0,
            "qkc": 0,
            "orr": 0,
            "sae": 0,
            "agent_controller": 0,
            "host": 0,
        },
        "sims": {},
        "dkms": {},
        "users": {},
        "qkcs": {},
        "orrs": {},
        "saes": {},
        "agent_controllers": {},
        "hosts": {},
    }


DEFAULT_STORE = _default_store()


class MemoryUnitOfWork:
    """UoW en memoria con almacen compartido."""

    def __init__(self, store: Optional[dict] = None) -> None:
        self._store = store if store is not None else DEFAULT_STORE
        self.repos = Repositories(
            simulations=SimulationRepoMemory(self._store),
            dkms=DKMSRepoMemory(self._store),
            qkcs=QKCRepoMemory(self._store),
            orrs=ORRRepoMemory(self._store),
            users=UserRepoMemory(self._store),
            saes=SAERepoMemory(self._store),
            agent_controllers=AgentControllerRepoMemory(self._store),
        )

    def __enter__(self) -> "MemoryUnitOfWork":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type:
            self.rollback()

    def commit(self) -> None:
        return None

    def rollback(self) -> None:
        return None
