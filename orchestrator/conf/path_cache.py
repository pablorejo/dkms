"""LRU+TTL cache compartido para respuestas del SDN ``/paths``.

Lo usan ``DKMS.sdn``, ``QKC.routing`` y ``ORR.routing``: los tres son
clientes del SDN y las mismas consultas pueden repetirse decenas de
veces por segundo durante un refill o relay multi-hop. Con un solo
pod SDN y N=50 DKMS golpeándolo, el controlador colapsa bajo carga
(minutes-scale latencies, 404 intermitentes). Una caché local TTL
corta reduce el tráfico a lo imprescindible sin cambiar la semántica:
los caminos cambian raramente y los TTLs negativos se recuperan en
segundos cuando un peer vuelve a estar disponible.
"""
from __future__ import annotations

import os
import threading
import time
from collections import OrderedDict
from typing import Any, Dict, List, Optional, Tuple


def _env_float(name: str, default: float, min_v: float, max_v: float) -> float:
    raw = (os.getenv(name) or "").strip()
    if not raw:
        return default
    try:
        value = float(raw)
    except ValueError:
        return default
    return max(min_v, min(value, max_v))


def _env_int(name: str, default: int, min_v: int, max_v: int) -> int:
    raw = (os.getenv(name) or "").strip()
    if not raw:
        return default
    try:
        value = int(raw)
    except ValueError:
        return default
    return max(min_v, min(value, max_v))


class PathCache:
    """LRU + TTL cache thread-safe con single-flight."""

    def __init__(
        self,
        *,
        positive_ttl_seconds: float,
        negative_ttl_seconds: float,
        max_entries: int,
    ) -> None:
        self._positive_ttl = float(positive_ttl_seconds)
        self._negative_ttl = float(negative_ttl_seconds)
        self._max_entries = max(16, int(max_entries))
        self._entries: "OrderedDict[Tuple, Tuple[Any, float]]" = OrderedDict()
        self._inflight: Dict[Tuple, threading.Event] = {}
        self._lock = threading.RLock()

    def get(self, key: Tuple) -> Optional[Any]:
        now = time.monotonic()
        with self._lock:
            entry = self._entries.get(key)
            if entry is None:
                return None
            value, expiry = entry
            if expiry <= now:
                self._entries.pop(key, None)
                return None
            self._entries.move_to_end(key)
            # Copia defensiva para listas/dicts:
            if isinstance(value, list):
                return list(value)
            if isinstance(value, dict):
                return dict(value)
            return value

    def put(self, key: Tuple, value: Any) -> None:
        is_negative = value is None or (isinstance(value, (list, dict)) and not value)
        ttl = self._negative_ttl if is_negative else self._positive_ttl
        expiry = time.monotonic() + ttl
        stored = value
        if isinstance(value, list):
            stored = list(value)
        elif isinstance(value, dict):
            stored = dict(value)
        with self._lock:
            self._entries[key] = (stored, expiry)
            self._entries.move_to_end(key)
            while len(self._entries) > self._max_entries:
                self._entries.popitem(last=False)

    def invalidate(self, key: Tuple) -> None:
        with self._lock:
            self._entries.pop(key, None)

    def acquire_inflight(self, key: Tuple) -> Tuple[threading.Event, bool]:
        """Returns (event, is_leader). Leader must call release_inflight."""
        with self._lock:
            event = self._inflight.get(key)
            if event is not None:
                return event, False
            event = threading.Event()
            self._inflight[key] = event
            return event, True

    def release_inflight(self, key: Tuple) -> None:
        with self._lock:
            event = self._inflight.pop(key, None)
        if event is not None:
            event.set()

    def clear(self) -> None:
        with self._lock:
            self._entries.clear()


def build_default_path_cache(prefix: str = "DKMS_SDN_PATH_CACHE") -> PathCache:
    """Construye un PathCache con TTLs configurables por env var.

    Env vars:
        - ``{prefix}_TTL_SECONDS`` (default 30)
        - ``{prefix}_NEGATIVE_TTL_SECONDS`` (default 3)
        - ``{prefix}_MAX_ENTRIES`` (default 4096)
    """
    return PathCache(
        positive_ttl_seconds=_env_float(f"{prefix}_TTL_SECONDS", 30.0, 0.0, 3600.0),
        negative_ttl_seconds=_env_float(f"{prefix}_NEGATIVE_TTL_SECONDS", 3.0, 0.0, 600.0),
        max_entries=_env_int(f"{prefix}_MAX_ENTRIES", 4096, 16, 1_000_000),
    )


__all__ = ["PathCache", "build_default_path_cache"]
