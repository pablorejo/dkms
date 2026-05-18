"""Unit tests for ``tests/cli/sat_parser``."""

from __future__ import annotations

import tempfile
from pathlib import Path

from tests.cli.sat_parser import (
    parse_generator_state,
    parse_log_file,
    strip_ansi,
)


REAL_LINE = (
    "2026-05-17T18:23:45.123456Z  INFO dkms::control::generator: "
    "generator.state peer=node-3 enc=4523 dec=4499 ack_pending=12 "
    'emit_total=98765 observed_keys_per_s="320.5" '
    'sdn_rate_keys_per_s="333.3"'
)


def test_parse_real_line_full_fields() -> None:
    r = parse_generator_state(REAL_LINE)
    assert r is not None
    assert r["peer"] == "node-3"
    assert r["enc"] == 4523
    assert r["dec"] == 4499
    assert r["ack_pending"] == 12
    assert r["emit_total"] == 98765
    assert r["observed_keys_per_s"] == 320.5
    assert r["sdn_rate_keys_per_s"] == 333.3
    assert r["t_log"].startswith("2026-05-17T18:23:45")


def test_parse_returns_none_for_non_matching() -> None:
    assert parse_generator_state("") is None
    assert parse_generator_state("random log line") is None
    assert parse_generator_state("INFO some.other.event peer=node-1") is None


def test_parse_returns_none_when_peer_missing() -> None:
    assert (
        parse_generator_state(
            "INFO generator.state enc=10 dec=5 emit_total=20"
        )
        is None
    )


def test_parse_tolerates_ansi_color_codes() -> None:
    line = (
        "\x1b[34m2026-05-17T18:23:45Z\x1b[0m INFO generator.state "
        "peer=\x1b[32mnode-9\x1b[0m enc=42 dec=0 ack_pending=0 "
        'emit_total=42 observed_keys_per_s="1.0" '
        'sdn_rate_keys_per_s="5.0"'
    )
    r = parse_generator_state(line)
    assert r is not None
    assert r["peer"] == "node-9"
    assert r["enc"] == 42


def test_parse_works_without_timestamp() -> None:
    line = (
        "INFO generator.state peer=node-2 enc=100 emit_total=50 "
        'observed_keys_per_s="10.0"'
    )
    r = parse_generator_state(line)
    assert r is not None
    assert r["peer"] == "node-2"
    assert "t_log" not in r


def test_parse_order_agnostic_fields() -> None:
    line = (
        'generator.state sdn_rate_keys_per_s="100.0" peer=x dec=0 '
        'enc=99 emit_total=99 ack_pending=0 observed_keys_per_s="77.0"'
    )
    r = parse_generator_state(line)
    assert r is not None
    assert r["peer"] == "x"
    assert r["enc"] == 99
    assert r["sdn_rate_keys_per_s"] == 100.0


def test_parse_skips_garbage_value_keeps_rest() -> None:
    line = "generator.state peer=node-1 enc=notanumber emit_total=10"
    r = parse_generator_state(line)
    assert r is not None
    assert r["peer"] == "node-1"
    assert "enc" not in r
    assert r["emit_total"] == 10


def test_parse_extra_unknown_fields_are_ignored() -> None:
    """Future-proofing: new fields should not break the parser."""
    line = (
        "generator.state peer=node-4 enc=1 dec=0 ack_pending=0 emit_total=1 "
        'observed_keys_per_s="1.0" sdn_rate_keys_per_s="2.0" '
        "priority=Important flow_id=node-4->node-5"
    )
    r = parse_generator_state(line)
    assert r is not None
    assert r["peer"] == "node-4"
    assert "priority" not in r
    assert "flow_id" not in r


def test_strip_ansi_removes_color_escapes() -> None:
    raw = "\x1b[31mred\x1b[0m and \x1b[1;33mbold\x1b[0m"
    assert strip_ansi(raw) == "red and bold"


def test_parse_log_file_reads_multi_event_log() -> None:
    text = "\n".join(
        [
            REAL_LINE,
            "garbage line that does not match",
            REAL_LINE.replace("node-3", "node-5"),
        ]
    )
    with tempfile.TemporaryDirectory() as tmpdir:
        log = Path(tmpdir) / "dkms.log"
        log.write_text(text + "\n", encoding="utf-8")
        events = parse_log_file(str(log))
    assert len(events) == 2
    peers = sorted(ev["peer"] for ev in events)
    assert peers == ["node-3", "node-5"]


def test_parse_log_file_handles_empty_file() -> None:
    with tempfile.TemporaryDirectory() as tmpdir:
        log = Path(tmpdir) / "empty.log"
        log.write_text("", encoding="utf-8")
        assert parse_log_file(str(log)) == []
