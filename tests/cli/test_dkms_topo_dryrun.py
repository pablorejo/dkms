"""Unit tests for the ``dkms_topo`` CLI entry point.

Covers:

* ``--dry-run`` smoke (OBJ-021 anticipated).
* Auto-naming convention per topology.
* ``_compute_loadtest_duration`` math.
* ``_ask_yes_no`` interactive helper (OBJ-020).
* ``_resolve_duplicate_sim`` confirmation logic (OBJ-020 / R-012).

All tests are pure-Python; no kubectl, no orchestator. Mocking is
done with ``unittest.mock``.
"""

from __future__ import annotations

import argparse
import io
import json
from unittest.mock import MagicMock, patch

import pytest

from tests.cli.dkms_topo import (
    _ask_yes_no,
    _auto_name,
    _compute_loadtest_duration,
    _resolve_duplicate_sim,
    run,
)
from tests.cli.orchestator_client import OrchestatorError


# -----------------------------------------------------------------------------
# Auto-naming
# -----------------------------------------------------------------------------


def _ns(**kw):
    return argparse.Namespace(**kw)


def test_auto_name_ring() -> None:
    assert _auto_name("ring", _ns(n=4)) == "ring-n4"


def test_auto_name_line() -> None:
    assert _auto_name("line", _ns(n=6)) == "line-n6"


def test_auto_name_mesh() -> None:
    assert _auto_name("mesh", _ns(n=3, m=4)) == "mesh-3x4"


def test_auto_name_star() -> None:
    assert _auto_name("star", _ns(p=2, b=3)) == "star-b3-p2"


def test_auto_name_random_with_seed() -> None:
    assert _auto_name("random", _ns(n=8, d=3.0, seed=42)) == "random-n8-d3.0-s42"


def test_auto_name_random_without_seed() -> None:
    name = _auto_name("random", _ns(n=5, d=2.5, seed=None))
    assert name.startswith("random-n5-d2.5-s")


# -----------------------------------------------------------------------------
# Loadtest duration derivation
# -----------------------------------------------------------------------------


def test_loadtest_duration_default_formula() -> None:
    # (100-10)/10 = 9 steps × 15s + 30s warmup + 60s margin = 225s
    ns = _ns(
        loadtest_duration=None,
        sae_start=10,
        sae_end=100,
        sae_step=10,
        sae_warmup=30.0,
        time_step=15.0,
    )
    assert _compute_loadtest_duration(ns) == 225.0


def test_loadtest_duration_explicit_override() -> None:
    ns = _ns(
        loadtest_duration=42.5,
        sae_start=0,
        sae_end=0,
        sae_step=1,
        sae_warmup=0.0,
        time_step=1.0,
    )
    assert _compute_loadtest_duration(ns) == 42.5


def test_loadtest_duration_zero_steps_ok() -> None:
    # end == start → 0 steps; just warmup + margin
    ns = _ns(
        loadtest_duration=None,
        sae_start=5,
        sae_end=5,
        sae_step=5,
        sae_warmup=10.0,
        time_step=15.0,
    )
    assert _compute_loadtest_duration(ns) == 70.0


# -----------------------------------------------------------------------------
# Dry-run path (OBJ-021)
# -----------------------------------------------------------------------------


def test_dry_run_ring_emits_valid_payload() -> None:
    stdout = io.StringIO()
    with patch("sys.stdout", stdout):
        rc = run(["ring", "-n", "4", "--dry-run"])
    assert rc == 0
    payload = json.loads(stdout.getvalue())
    assert payload["name"] == "ring-n4"
    assert len(payload["nodes"]) == 4
    assert len(payload["links"]) == 4
    for ln in payload["links"]:
        assert ln["link_type"] == "QKD"
        assert ln["quditto_rate_r0"] == 2000.0
        assert ln["quditto_rate_alpha"] == 0.0


def test_dry_run_with_overrides() -> None:
    stdout = io.StringIO()
    with patch("sys.stdout", stdout):
        rc = run(
            [
                "mesh",
                "-n",
                "2",
                "-m",
                "3",
                "--r0",
                "1500",
                "--alpha",
                "0.3",
                "--distance",
                "10",
                "--buffer-enc-size",
                "1024",
                "--name",
                "my-mesh",
                "--dry-run",
            ]
        )
    assert rc == 0
    payload = json.loads(stdout.getvalue())
    assert payload["name"] == "my-mesh"
    assert len(payload["nodes"]) == 6
    for ln in payload["links"]:
        assert ln["quditto_rate_r0"] == 1500.0
        assert ln["quditto_rate_alpha"] == 0.3
        assert ln["distance_km"] == 10
        assert ln["quditto_max_buffer_size"] == 1024


def test_dry_run_invalid_args_returns_one() -> None:
    rc = run(["ring", "-n", "2", "--dry-run"])  # ring requires n >= 3
    assert rc == 1


def test_no_flag_returns_two() -> None:
    rc = run(["ring", "-n", "4"])  # no --dry-run, no --buffer-saturated, no --sae-test
    assert rc == 2


# -----------------------------------------------------------------------------
# _ask_yes_no
# -----------------------------------------------------------------------------


def test_ask_yes_no_non_tty_returns_false() -> None:
    with patch("sys.stdin") as stdin:
        stdin.isatty.return_value = False
        assert _ask_yes_no("anything?") is False


def test_ask_yes_no_explicit_y() -> None:
    with patch("sys.stdin") as stdin, patch("builtins.input", return_value="y"):
        stdin.isatty.return_value = True
        assert _ask_yes_no("ok?") is True


def test_ask_yes_no_explicit_yes() -> None:
    with patch("sys.stdin") as stdin, patch("builtins.input", return_value="YES"):
        stdin.isatty.return_value = True
        assert _ask_yes_no("ok?") is True


def test_ask_yes_no_empty_defaults_to_no() -> None:
    with patch("sys.stdin") as stdin, patch("builtins.input", return_value=""):
        stdin.isatty.return_value = True
        assert _ask_yes_no("ok?") is False


def test_ask_yes_no_n() -> None:
    with patch("sys.stdin") as stdin, patch("builtins.input", return_value="n"):
        stdin.isatty.return_value = True
        assert _ask_yes_no("ok?") is False


# -----------------------------------------------------------------------------
# _resolve_duplicate_sim
# -----------------------------------------------------------------------------


def test_resolve_duplicate_no_match_returns_zero() -> None:
    client = MagicMock()
    client.list_simulations.return_value = [{"id": 1, "name": "other"}]
    assert _resolve_duplicate_sim(client, "no-such", force=True) == 0
    client.delete_simulation.assert_not_called()


def test_resolve_duplicate_force_deletes_all() -> None:
    client = MagicMock()
    client.list_simulations.return_value = [
        {"id": 7, "name": "my-sim"},
        {"id": 8, "name": "my-sim"},
        {"id": 9, "name": "other"},
    ]
    deleted = _resolve_duplicate_sim(client, "my-sim", force=True)
    assert deleted == 2
    deleted_ids = sorted(
        call.args[0] for call in client.delete_simulation.call_args_list
    )
    assert deleted_ids == [7, 8]


def test_resolve_duplicate_interactive_yes_deletes() -> None:
    client = MagicMock()
    client.list_simulations.return_value = [{"id": 1, "name": "my-sim"}]
    with patch(
        "tests.cli.dkms_topo._ask_yes_no", return_value=True
    ):
        deleted = _resolve_duplicate_sim(client, "my-sim", force=False)
    assert deleted == 1
    client.delete_simulation.assert_called_once_with(1)


def test_resolve_duplicate_interactive_no_aborts_with_systemexit() -> None:
    client = MagicMock()
    client.list_simulations.return_value = [{"id": 5, "name": "my-sim"}]
    with patch(
        "tests.cli.dkms_topo._ask_yes_no", return_value=False
    ):
        with pytest.raises(SystemExit) as ei:
            _resolve_duplicate_sim(client, "my-sim", force=False)
    assert ei.value.code == 5
    client.delete_simulation.assert_not_called()


def test_resolve_duplicate_list_failure_does_not_abort() -> None:
    """If list_simulations fails (e.g. orch unreachable), warn and continue."""
    client = MagicMock()
    client.list_simulations.side_effect = OrchestatorError(
        500, None, "orch unreachable"
    )
    deleted = _resolve_duplicate_sim(client, "anything", force=True)
    assert deleted == 0


def test_resolve_duplicate_delete_failure_continues() -> None:
    """Per-sim delete failures don't stop the loop; we log and move on."""
    client = MagicMock()
    client.list_simulations.return_value = [
        {"id": 1, "name": "my-sim"},
        {"id": 2, "name": "my-sim"},
    ]

    def delete_side(sim_id):
        if sim_id == 1:
            raise OrchestatorError(500, None, "boom")
        return {"status": "deleted"}

    client.delete_simulation.side_effect = delete_side
    assert _resolve_duplicate_sim(client, "my-sim", force=True) == 1
