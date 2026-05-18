"""Unit tests for ``tests/cli/orchestator_client``.

All network calls are mocked via ``unittest.mock``. These tests do NOT
require port-forwards or a running orchestator.
"""

from __future__ import annotations

from typing import Any
from unittest.mock import MagicMock, patch

import pytest
import requests

from tests.cli.orchestator_client import (
    AlreadyExistsError,
    AuthenticationError,
    BadRequestError,
    NetworkError,
    NotFoundError,
    OrchestatorClient,
    OrchestatorError,
    ServerError,
)


# -----------------------------------------------------------------------------
# Helpers
# -----------------------------------------------------------------------------


def _make_response(
    status: int = 200, body: Any = None, *, raw_text: str | None = None
) -> MagicMock:
    """Build a mocked ``requests.Response``."""
    resp = MagicMock(spec=requests.Response)
    resp.status_code = status
    if raw_text is not None:
        resp.content = raw_text.encode()
        resp.text = raw_text
        resp.json.side_effect = ValueError("not json")
    elif body is None:
        resp.content = b""
        resp.json.side_effect = ValueError("empty")
    else:
        import json as _json

        encoded = _json.dumps(body).encode()
        resp.content = encoded
        resp.text = encoded.decode()
        resp.json.return_value = body
    return resp


def _client(max_retries: int = 1, backoff_base: float = 0.0) -> OrchestatorClient:
    """Client with retries minimized (and zero backoff) so tests stay fast."""
    return OrchestatorClient(
        authz_url="http://a:1",
        orch_url="http://b:2",
        max_retries=max_retries,
        backoff_base=backoff_base,
    )


# -----------------------------------------------------------------------------
# Login / register
# -----------------------------------------------------------------------------


def test_login_parses_token_and_calls_me() -> None:
    c = _client()
    responses = [
        _make_response(200, {"access_token": "tok-abc", "expires_in": 3600}),
        _make_response(200, {"id": 7, "username": "u", "email": "u@x", "is_active": True}),
    ]
    with patch.object(c.session, "request", side_effect=responses) as req:
        token, uid = c.login("u", "p")
    assert token == "tok-abc"
    assert uid == 7
    assert c.token == "tok-abc"
    assert c.uid == 7
    # First call /login, second /me with bearer
    args1, kwargs1 = req.call_args_list[0]
    assert args1[0] == "POST" and args1[1].endswith("/login")
    args2, kwargs2 = req.call_args_list[1]
    assert args2[0] == "GET" and args2[1].endswith("/me")
    assert kwargs2["headers"]["Authorization"] == "Bearer tok-abc"


def test_login_invalid_credentials_raises_authentication() -> None:
    c = _client()
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(401, {"detail": "Invalid credentials"}),
    ):
        with pytest.raises(AuthenticationError) as ei:
            c.login("u", "bad")
    assert ei.value.status_code == 401


def test_register_user_conflict_raises_already_exists() -> None:
    c = _client()
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(409, {"detail": "Username already exists"}),
    ):
        with pytest.raises(AlreadyExistsError):
            c.register_user("u", "u@x", "p")


def test_login_without_token_raises_orchestator_error() -> None:
    c = _client()
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(200, {"expires_in": 3600}),
    ):
        with pytest.raises(OrchestatorError):
            c.login("u", "p")


# -----------------------------------------------------------------------------
# Headers / auth required for orch endpoints
# -----------------------------------------------------------------------------


def test_orch_call_without_login_raises_authentication() -> None:
    c = _client()
    with pytest.raises(AuthenticationError):
        c.list_simulations()


def test_create_simulation_sends_x_user_id_header() -> None:
    c = _client()
    c.token = "t"
    c.uid = 3
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(201, {"id": 99, "name": "test"}),
    ) as req:
        sim_id = c.create_simulation({"name": "test"})
    assert sim_id == 99
    _, kwargs = req.call_args
    assert kwargs["headers"]["X-User-Id"] == "3"


def test_run_simulation_sends_x_simulation_id_header() -> None:
    c = _client()
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(201, {"id": 1, "simulation_id": 7, "status": "QUEUED", "created_at": "now"}),
    ) as req:
        c.run_simulation(7)
    _, kwargs = req.call_args
    assert kwargs["headers"]["X-Simulation-Id"] == "7"
    assert kwargs["headers"]["X-User-Id"] == "1"


# -----------------------------------------------------------------------------
# Retries
# -----------------------------------------------------------------------------


def test_5xx_is_retried_until_success() -> None:
    c = _client(max_retries=3)
    c.uid = 1
    responses = [
        _make_response(503, {"detail": "unavailable"}),
        _make_response(500, {"detail": "boom"}),
        _make_response(200, []),
    ]
    with patch.object(c.session, "request", side_effect=responses) as req:
        out = c.list_simulations()
    assert out == []
    assert req.call_count == 3


def test_5xx_exhausted_raises_server_error() -> None:
    c = _client(max_retries=2)
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(500, {"detail": "boom"}),
    ) as req:
        with pytest.raises(ServerError):
            c.list_simulations()
    # 2 retries means 3 total attempts
    assert req.call_count == 3


def test_4xx_is_not_retried() -> None:
    c = _client(max_retries=5)
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(404, {"detail": "no sim"}),
    ) as req:
        with pytest.raises(NotFoundError):
            c.get_simulation(123)
    assert req.call_count == 1


def test_mutating_post_does_not_retry_on_5xx() -> None:
    """``create_simulation`` is non-idempotent → ``retriable=False``."""
    c = _client(max_retries=5)
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(500, {"detail": "boom"}),
    ) as req:
        with pytest.raises(ServerError):
            c.create_simulation({"name": "x"})
    # No retries for create/run/stop/submit
    assert req.call_count == 1


def test_network_error_is_retried() -> None:
    c = _client(max_retries=2)
    c.uid = 1
    side_effects = [
        requests.ConnectionError("connect refused"),
        requests.Timeout("read timeout"),
        _make_response(200, []),
    ]
    with patch.object(c.session, "request", side_effect=side_effects) as req:
        out = c.list_simulations()
    assert out == []
    assert req.call_count == 3


def test_network_error_exhausted_raises_network_error() -> None:
    c = _client(max_retries=1)
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        side_effect=requests.ConnectionError("nope"),
    ) as req:
        with pytest.raises(NetworkError):
            c.list_simulations()
    assert req.call_count == 2


# -----------------------------------------------------------------------------
# Error body extraction
# -----------------------------------------------------------------------------


def test_error_message_from_detail_field() -> None:
    c = _client()
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(400, {"detail": "missing field bar"}),
    ):
        with pytest.raises(BadRequestError) as ei:
            c.create_simulation({"name": "x"})
    assert "missing field bar" in ei.value.message


def test_error_message_fallback_when_no_json() -> None:
    c = _client()
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(500, None, raw_text="<html>boom</html>"),
    ):
        with pytest.raises(ServerError) as ei:
            c.list_simulations()
    # No JSON body; falls back to "HTTP 500"
    assert ei.value.status_code == 500


# -----------------------------------------------------------------------------
# Round-trip parsing for typical endpoints
# -----------------------------------------------------------------------------


def test_list_simulations_returns_list() -> None:
    c = _client()
    c.uid = 1
    with patch.object(
        c.session,
        "request",
        return_value=_make_response(200, [{"id": 1, "name": "a"}, {"id": 2, "name": "b"}]),
    ):
        out = c.list_simulations()
    assert len(out) == 2
    assert out[0]["id"] == 1


def test_submit_loadtest_returns_dict() -> None:
    c = _client()
    c.uid = 1
    payload = {
        "test_id": "ramp-x",
        "deployment_name": "loadtest-x",
        "simulation_id": 4,
        "grafana_url": "https://...",
        "replicas": 1,
        "ready_replicas": 0,
        "available_replicas": 0,
        "created_at": None,
    }
    with patch.object(
        c.session, "request", return_value=_make_response(201, payload)
    ):
        out = c.submit_loadtest(4, {"sae_start": 10})
    assert out["test_id"] == "ramp-x"
