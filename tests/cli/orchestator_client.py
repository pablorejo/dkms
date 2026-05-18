"""HTTP client for the ``orchestator`` API and the ``authz`` service.

Both endpoints are reached through the user's local port-forwards
(default ``authz`` on ``:18081`` and ``orchestator`` on ``:18080``).
The client is intentionally small and dependency-light (only
``requests`` + stdlib), so it works without a venv when ``requests``
is already installed system-wide.

Auth flow:

1. ``register_user`` (idempotent if username exists -> raises
   ``AlreadyExistsError``, caller may catch and proceed to login).
2. ``login`` obtains the JWT and resolves the user id via ``/me``.
3. Subsequent orchestator calls include ``X-User-Id`` automatically.

Retries:

Idempotent verbs (``GET``, ``DELETE``) and HTTP 5xx / network errors
trigger up to ``max_retries`` retries with exponential backoff. 4xx
status codes are NEVER retried — they surface as a typed
``OrchestatorError`` subclass.
"""

from __future__ import annotations

import json
import logging
import time
from dataclasses import dataclass
from typing import Any

import requests

LOG = logging.getLogger(__name__)

DEFAULT_AUTHZ_URL = "http://127.0.0.1:18081"
DEFAULT_ORCH_URL = "http://127.0.0.1:18080"
DEFAULT_TIMEOUT = 30.0
DEFAULT_MAX_RETRIES = 3
DEFAULT_BACKOFF_BASE = 0.5  # seconds; doubled per retry


# -----------------------------------------------------------------------------
# Typed errors
# -----------------------------------------------------------------------------


class OrchestatorError(Exception):
    """Base error class for orchestator/authz HTTP failures."""

    def __init__(self, status_code: int, body: Any, message: str) -> None:
        super().__init__(message)
        self.status_code = status_code
        self.body = body
        self.message = message

    def __repr__(self) -> str:
        return f"{type(self).__name__}(status={self.status_code}, message={self.message!r})"


class AuthenticationError(OrchestatorError):
    """401 — invalid credentials or expired/missing token."""


class PermissionDeniedError(OrchestatorError):
    """403 — authenticated but not allowed."""


class NotFoundError(OrchestatorError):
    """404 — resource missing."""


class AlreadyExistsError(OrchestatorError):
    """409 — conflict (e.g. duplicate user, sim already running)."""


class BadRequestError(OrchestatorError):
    """400/422 — client-side problem."""


class ServerError(OrchestatorError):
    """5xx — server-side failure (retried by default)."""


class NetworkError(OrchestatorError):
    """Connection/timeout failures with no HTTP status."""

    def __init__(self, message: str) -> None:
        super().__init__(status_code=0, body=None, message=message)


def _classify(status_code: int, body: Any, message: str) -> OrchestatorError:
    if status_code == 401:
        return AuthenticationError(status_code, body, message)
    if status_code == 403:
        return PermissionDeniedError(status_code, body, message)
    if status_code == 404:
        return NotFoundError(status_code, body, message)
    if status_code == 409:
        return AlreadyExistsError(status_code, body, message)
    if status_code in (400, 422):
        return BadRequestError(status_code, body, message)
    if 500 <= status_code < 600:
        return ServerError(status_code, body, message)
    return OrchestatorError(status_code, body, message)


# -----------------------------------------------------------------------------
# Client
# -----------------------------------------------------------------------------


@dataclass
class _Endpoint:
    method: str
    url: str
    headers: dict[str, str]
    json_body: Any = None
    params: dict[str, Any] | None = None


class OrchestatorClient:
    """Thin HTTP client for ``orchestator`` + ``authz``."""

    def __init__(
        self,
        authz_url: str = DEFAULT_AUTHZ_URL,
        orch_url: str = DEFAULT_ORCH_URL,
        timeout: float = DEFAULT_TIMEOUT,
        max_retries: int = DEFAULT_MAX_RETRIES,
        backoff_base: float = DEFAULT_BACKOFF_BASE,
        session: requests.Session | None = None,
    ) -> None:
        self.authz_url = authz_url.rstrip("/")
        self.orch_url = orch_url.rstrip("/")
        self.timeout = timeout
        self.max_retries = max_retries
        self.backoff_base = backoff_base
        self.session = session if session is not None else requests.Session()
        self.token: str | None = None
        self.uid: int | None = None

    # ----- internals ------------------------------------------------------

    def _request(
        self,
        method: str,
        url: str,
        *,
        json_body: Any = None,
        headers: dict[str, str] | None = None,
        params: dict[str, Any] | None = None,
        retriable: bool = True,
    ) -> Any:
        """Execute ``method url`` with retries, raise typed errors on failure."""
        attempts = self.max_retries + 1 if retriable else 1
        last_exc: Exception | None = None
        for attempt in range(attempts):
            try:
                resp = self.session.request(
                    method,
                    url,
                    json=json_body,
                    headers=headers,
                    params=params,
                    timeout=self.timeout,
                )
            except (requests.ConnectionError, requests.Timeout) as exc:
                last_exc = NetworkError(f"{method} {url}: {exc}")
                LOG.warning("network error attempt %d/%d: %s", attempt + 1, attempts, exc)
                if attempt + 1 == attempts:
                    raise last_exc
                time.sleep(self.backoff_base * (2**attempt))
                continue

            status_code = resp.status_code
            body: Any
            try:
                body = resp.json() if resp.content else None
            except (json.JSONDecodeError, ValueError):
                body = resp.text

            if 200 <= status_code < 300:
                return body

            message = self._extract_message(body) or f"HTTP {status_code}"
            err = _classify(status_code, body, message)

            # 4xx -> never retry. 5xx -> retry only for idempotent verbs.
            if 500 <= status_code < 600 and retriable and attempt + 1 < attempts:
                LOG.warning(
                    "%d on %s %s attempt %d/%d, retrying",
                    status_code, method, url, attempt + 1, attempts,
                )
                time.sleep(self.backoff_base * (2**attempt))
                continue
            raise err

        # Unreachable, but keeps mypy happy.
        if last_exc:
            raise last_exc
        raise OrchestatorError(0, None, "request loop ended without response")

    @staticmethod
    def _extract_message(body: Any) -> str | None:
        if isinstance(body, dict):
            for key in ("detail", "message", "error"):
                val = body.get(key)
                if isinstance(val, str) and val:
                    return val
        return None

    def _orch_headers(self, extra: dict[str, str] | None = None) -> dict[str, str]:
        if self.uid is None:
            raise AuthenticationError(401, None, "Not logged in; call login() first")
        headers = {"X-User-Id": str(self.uid)}
        if extra:
            headers.update(extra)
        return headers

    # ----- authz ---------------------------------------------------------

    def register_user(self, username: str, email: str, password: str) -> tuple[str, int]:
        """POST /register on authz. Returns (token, uid)."""
        body = self._request(
            "POST",
            f"{self.authz_url}/register",
            json_body={"username": username, "email": email, "password": password},
            retriable=False,
        )
        token = body.get("access_token") if isinstance(body, dict) else None
        if not token:
            raise OrchestatorError(0, body, "register: missing access_token in response")
        uid = self._whoami(token)
        self.token = token
        self.uid = uid
        return token, uid

    def login(self, username: str, password: str) -> tuple[str, int]:
        """POST /login on authz. Returns (token, uid)."""
        body = self._request(
            "POST",
            f"{self.authz_url}/login",
            json_body={"username": username, "password": password},
            retriable=False,
        )
        token = body.get("access_token") if isinstance(body, dict) else None
        if not token:
            raise OrchestatorError(0, body, "login: missing access_token in response")
        uid = self._whoami(token)
        self.token = token
        self.uid = uid
        return token, uid

    def _whoami(self, token: str) -> int:
        body = self._request(
            "GET",
            f"{self.authz_url}/me",
            headers={"Authorization": f"Bearer {token}"},
            retriable=True,
        )
        if not isinstance(body, dict) or "id" not in body:
            raise OrchestatorError(0, body, "/me: missing id in response")
        return int(body["id"])

    # ----- orchestator ---------------------------------------------------

    def list_simulations(self) -> list[dict[str, Any]]:
        body = self._request(
            "GET",
            f"{self.orch_url}/orch/web/simulations",
            headers=self._orch_headers(),
            retriable=True,
        )
        return list(body) if body else []

    def create_simulation(self, payload: dict[str, Any]) -> int:
        body = self._request(
            "POST",
            f"{self.orch_url}/orch/web/simulations",
            json_body=payload,
            headers=self._orch_headers(),
            retriable=False,
        )
        if not isinstance(body, dict) or "id" not in body:
            raise OrchestatorError(0, body, "create_simulation: missing id in response")
        return int(body["id"])

    def get_simulation(self, sim_id: int) -> dict[str, Any]:
        body = self._request(
            "GET",
            f"{self.orch_url}/orch/web/simulations/{sim_id}",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=True,
        )
        if not isinstance(body, dict):
            raise OrchestatorError(0, body, "get_simulation: non-dict response")
        return body

    def run_simulation(self, sim_id: int) -> dict[str, Any]:
        body = self._request(
            "POST",
            f"{self.orch_url}/orch/web/simulations/{sim_id}/run",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=False,
        )
        if not isinstance(body, dict):
            raise OrchestatorError(0, body, "run_simulation: non-dict response")
        return body

    def stop_simulation(self, sim_id: int) -> dict[str, Any]:
        body = self._request(
            "POST",
            f"{self.orch_url}/orch/web/simulations/{sim_id}/stop",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=False,
        )
        if not isinstance(body, dict):
            raise OrchestatorError(0, body, "stop_simulation: non-dict response")
        return body

    def delete_simulation(self, sim_id: int) -> dict[str, Any]:
        body = self._request(
            "DELETE",
            f"{self.orch_url}/orch/web/simulations/{sim_id}",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=True,
        )
        if not isinstance(body, dict):
            return {"status": "deleted"}
        return body

    def submit_loadtest(self, sim_id: int, params: dict[str, Any]) -> dict[str, Any]:
        body = self._request(
            "POST",
            f"{self.orch_url}/orch/web/simulations/{sim_id}/tests",
            json_body=params,
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=False,
        )
        if not isinstance(body, dict):
            raise OrchestatorError(0, body, "submit_loadtest: non-dict response")
        return body

    def list_loadtests(self, sim_id: int) -> list[dict[str, Any]]:
        body = self._request(
            "GET",
            f"{self.orch_url}/orch/web/simulations/{sim_id}/tests",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=True,
        )
        return list(body) if body else []

    def stop_loadtest(self, sim_id: int, test_id: str) -> dict[str, Any]:
        body = self._request(
            "DELETE",
            f"{self.orch_url}/orch/web/simulations/{sim_id}/tests/{test_id}",
            headers=self._orch_headers({"X-Simulation-Id": str(sim_id)}),
            retriable=True,
        )
        return body if isinstance(body, dict) else {"status": "deleted"}


__all__ = [
    "AlreadyExistsError",
    "AuthenticationError",
    "BadRequestError",
    "DEFAULT_AUTHZ_URL",
    "DEFAULT_BACKOFF_BASE",
    "DEFAULT_MAX_RETRIES",
    "DEFAULT_ORCH_URL",
    "DEFAULT_TIMEOUT",
    "NetworkError",
    "NotFoundError",
    "OrchestatorClient",
    "OrchestatorError",
    "PermissionDeniedError",
    "ServerError",
]
