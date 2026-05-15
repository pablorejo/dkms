from __future__ import annotations

import os
from typing import Optional

from .ports import UnitOfWork

from dotenv import load_dotenv

load_dotenv()

def build_uow_from_env(database_url: Optional[str] = None) -> UnitOfWork:
    """Crea la UoW segun la variable PERSISTENCE_BACKEND.

    - memory: usa un almacen en memoria.
    - sqlalchemy: usa DB_URL (o fallback a database_url).
    """
    backend = os.getenv("PERSISTENCE_BACKEND", "sqlalchemy").lower()

    if backend == "memory":
        from .memory.uow import MemoryUnitOfWork

        return MemoryUnitOfWork()

    if backend == "sqlalchemy":
        db_url = os.getenv("DB_URL") or database_url
        if not db_url:
            raise ValueError("DB_URL debe definirse para usar el backend sqlalchemy")
        from .sqlalchemy.uow import SqlAlchemyUnitOfWork

        return SqlAlchemyUnitOfWork.from_url(db_url)

    raise ValueError(f"Backend de persistencia no soportado: {backend}")
