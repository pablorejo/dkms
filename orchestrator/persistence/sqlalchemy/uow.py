from __future__ import annotations

import os
import time
from functools import lru_cache
from typing import Optional

from sqlalchemy import create_engine, text
from sqlalchemy.exc import OperationalError
from sqlalchemy.orm import Session, sessionmaker

from ..ports import Repositories
from .repos import (
    AgentControllerRepoSA,
    DKMSRepoSA,
    ORRRepoSA,
    QKCRepoSA,
    SAERepoSA,
    SimulationRepoSA,
    UserRepoSA,
)


def _env_int(name: str, default: int) -> int:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        return int(raw)
    except ValueError:
        return default


def _env_float(name: str, default: float) -> float:
    raw = os.getenv(name)
    if raw is None:
        return default
    try:
        return float(raw)
    except ValueError:
        return default


def _env_bool(name: str, default: bool) -> bool:
    raw = os.getenv(name)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def _is_postgres_url(database_url: str) -> bool:
    normalized = (database_url or "").strip().lower()
    return normalized.startswith("postgresql://") or normalized.startswith("postgresql+")


def _engine_kwargs(database_url: str) -> dict:
    kwargs = {
        "pool_pre_ping": _env_bool("SQLALCHEMY_POOL_PRE_PING", True),
        "pool_recycle": _env_int("SQLALCHEMY_POOL_RECYCLE_SECONDS", 300),
    }

    if _is_postgres_url(database_url):
        # QueuePool knobs. Not valid for SQLite (NullPool), hence the gate.
        # LIFO lets the tail of the pool go idle and age out via
        # ``pool_recycle`` — important with N-replica DKMS deployments
        # sharing a single RDS, where default FIFO keeps every slot warm.
        kwargs["pool_size"] = max(1, _env_int("SQLALCHEMY_POOL_SIZE", 5))
        kwargs["max_overflow"] = max(0, _env_int("SQLALCHEMY_MAX_OVERFLOW", 10))
        kwargs["pool_timeout"] = _env_int("SQLALCHEMY_POOL_TIMEOUT_SECONDS", 10)
        kwargs["pool_use_lifo"] = _env_bool("SQLALCHEMY_POOL_USE_LIFO", True)
        kwargs["connect_args"] = {
            "connect_timeout": _env_int("SQLALCHEMY_CONNECT_TIMEOUT_SECONDS", 5),
        }
    return kwargs


@lru_cache(maxsize=8)
def _session_factory_for_url(database_url: str) -> sessionmaker:
    engine = create_engine(database_url, **_engine_kwargs(database_url))
    return sessionmaker(autocommit=False, autoflush=False, bind=engine)


def _is_transient_operational_error(exc: OperationalError) -> bool:
    message = str(exc).lower()
    tokens = (
        "connection failed",
        "could not connect",
        "connection refused",
        "server closed",
        "ssl error",
        "unexpected eof",
        "timed out",
        "timeout",
    )
    return any(token in message for token in tokens)


class SqlAlchemyUnitOfWork:
    """UoW basado en SQLAlchemy que controla la sesion y transacciones."""

    def __init__(self, session_factory: sessionmaker) -> None:
        self._session_factory = session_factory
        self._session: Optional[Session] = None
        self._repos: Optional[Repositories] = None

    @classmethod
    def from_url(cls, database_url: str) -> "SqlAlchemyUnitOfWork":
        return cls(_session_factory_for_url(database_url))

    @property
    def repos(self) -> Repositories:
        if self._repos is None:
            raise RuntimeError("La UnitOfWork no ha sido inicializada")
        return self._repos

    def __enter__(self) -> "SqlAlchemyUnitOfWork":
        connect_retries = max(0, _env_int("SQLALCHEMY_CONNECT_RETRIES", 2))
        retry_delay_seconds = max(0.0, _env_float("SQLALCHEMY_CONNECT_RETRY_DELAY_SECONDS", 0.25))

        self._session = None
        for attempt in range(connect_retries + 1):
            session = self._session_factory()
            try:
                # Force an immediate checkout so transient TLS/DB failures can be retried.
                session.execute(text("SELECT 1"))
                self._session = session
                break
            except OperationalError as exc:
                session.close()
                if attempt >= connect_retries or not _is_transient_operational_error(exc):
                    raise
                time.sleep(retry_delay_seconds * (attempt + 1))

        if self._session is None:
            raise RuntimeError("No se pudo abrir sesion SQLAlchemy")

        self._repos = Repositories(
            simulations=SimulationRepoSA(self._session),
            dkms=DKMSRepoSA(self._session),
            qkcs=QKCRepoSA(self._session),
            orrs=ORRRepoSA(self._session),
            users=UserRepoSA(self._session),
            saes=SAERepoSA(self._session),
            agent_controllers=AgentControllerRepoSA(self._session),
        )
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        if exc_type:
            self.rollback()
        if self._session is not None:
            self._session.close()
            self._session = None
            self._repos = None

    def commit(self) -> None:
        if self._session is None:
            raise RuntimeError("No hay sesion activa para commit")
        self._session.commit()

    def rollback(self) -> None:
        if self._session is None:
            return
        self._session.rollback()
