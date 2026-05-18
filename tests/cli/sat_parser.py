"""Parse ``generator.state`` log lines emitted by the Rust DKMS.

The Rust DKMS' generator emits one ``info!`` per peer every 5 s with
the buffer fill, ack pending count, total keys emitted, observed rate
and the rate currently assigned by the SDN (see
``dkms/src/control/generator.rs:263-272``)::

    2026-05-17T18:23:45.123456Z  INFO dkms::control::generator:
    generator.state peer=node-3 enc=4523 dec=4499 ack_pending=12
    emit_total=98765 observed_keys_per_s="320.5" sdn_rate_keys_per_s="333.3"

The parser is **order-agnostic** and tolerates ANSI color codes, the
``tracing`` "pretty" formatter, and the JSON formatter (``"fields":{
... "peer":"node-3" ...}``). Returns ``None`` for any line that does
not contain a ``generator.state`` event or that lacks the ``peer`` key.
"""

from __future__ import annotations

import re
from typing import Any

# Catches the "tracing" key=value form. Value may be quoted or bare.
# Allowed value chars (bare): anything but whitespace or commas.
_KV_RE = re.compile(r'(\w+)=(?:"([^"]*)"|([^\s,}]+))')

# ISO-8601 timestamp at the start of a line (with optional fractional seconds
# and timezone). Also accepts "Z" or "+HH:MM".
_TS_RE = re.compile(
    r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)"
)

# ANSI escape codes (color/format) emitted by the default tracing subscriber.
_ANSI_RE = re.compile(r"\x1b\[[0-9;]*m")

_GENERATOR_STATE_MARKER = "generator.state"

_INT_FIELDS = {"enc", "dec", "ack_pending", "emit_total"}
_FLOAT_FIELDS = {"observed_keys_per_s", "sdn_rate_keys_per_s"}
_STR_FIELDS = {"peer"}


def strip_ansi(line: str) -> str:
    """Remove ANSI escape codes that some terminals/loggers add."""
    return _ANSI_RE.sub("", line)


def parse_generator_state(line: str) -> dict[str, Any] | None:
    """Parse a ``generator.state`` log line into a dict, or return ``None``.

    Returned keys (only those present in the line):

    * ``peer`` (str) — destination DKMS uid.
    * ``enc`` (int) — current ENC buffer length.
    * ``dec`` (int) — current DEC buffer length.
    * ``ack_pending`` (int) — keys awaiting ACK.
    * ``emit_total`` (int) — cumulative keys emitted to this peer.
    * ``observed_keys_per_s`` (float) — empirical emission rate over
      the last ``rate_refresh_ms`` window.
    * ``sdn_rate_keys_per_s`` (float) — rate currently assigned by SDN.
    * ``t_log`` (str) — ISO-8601 timestamp at the start of the line,
      if present.

    Returns ``None`` for lines that:

    * Do not contain ``generator.state``, OR
    * Do not yield a ``peer=`` field after parsing.
    """
    if _GENERATOR_STATE_MARKER not in line:
        return None

    clean = strip_ansi(line)

    out: dict[str, Any] = {}

    ts = _TS_RE.search(clean)
    if ts:
        out["t_log"] = ts.group(1)

    for match in _KV_RE.finditer(clean):
        key = match.group(1)
        value = match.group(2) if match.group(2) is not None else match.group(3)
        if key in _STR_FIELDS:
            out[key] = value
        elif key in _INT_FIELDS:
            try:
                out[key] = int(value)
            except ValueError:
                continue
        elif key in _FLOAT_FIELDS:
            try:
                out[key] = float(value)
            except ValueError:
                continue

    if "peer" not in out:
        return None

    return out


def parse_log_file(path: str) -> list[dict[str, Any]]:
    """Read a log file and return every parsed ``generator.state`` event."""
    out: list[dict[str, Any]] = []
    with open(path, "r", encoding="utf-8", errors="replace") as fh:
        for raw in fh:
            parsed = parse_generator_state(raw)
            if parsed is not None:
                out.append(parsed)
    return out


__all__ = [
    "parse_generator_state",
    "parse_log_file",
    "strip_ansi",
]
