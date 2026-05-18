"""Assemble a complete ``WebSimulationUpsertRequest`` body.

The orchestator expects (see ``orchestrator/api_orchestator.py:189``)::

    {
        "name": "<sim name>",
        "description": "<optional>",
        "sdn":   {"ip": "...", "port": 3000, "type_http": "http"},
        "nodes": [WebNodeInput...],
        "links": [WebLinkInput...],
    }

The ``nodes`` and ``links`` come from a topology builder
(``tests/cli/topology_builders``). This module fills the wrapper and
applies any per-link overrides the caller passes (``r0``, ``alpha``,
``distance_km``, ``buffer_max``).

Validation is performed manually (no ``pydantic`` dependency): missing
required fields raise ``ValueError`` so the caller gets a fast error
before the request even hits the network.
"""

from __future__ import annotations

from typing import Any

DEFAULT_SDN_IP: str = "172.30.0.2"
DEFAULT_SDN_PORT: int = 3000
DEFAULT_SDN_TYPE_HTTP: str = "http"

REQUIRED_NODE_FIELDS = (
    "uid",
    "node_id",
    "label",
    "x",
    "y",
)
REQUIRED_LINK_FIELDS = (
    "uid",
    "source_uid",
    "target_uid",
    "link_type",
    "distance_km",
    "quditto_rate_r0",
    "quditto_rate_alpha",
    "quditto_max_buffer_size",
)


def _normalize_sdn(sdn_endpoint: str | dict[str, Any] | None) -> dict[str, Any]:
    if sdn_endpoint is None:
        return {
            "ip": DEFAULT_SDN_IP,
            "port": DEFAULT_SDN_PORT,
            "type_http": DEFAULT_SDN_TYPE_HTTP,
        }
    if isinstance(sdn_endpoint, dict):
        out: dict[str, Any] = {
            "ip": str(sdn_endpoint.get("ip", DEFAULT_SDN_IP)),
            "port": int(sdn_endpoint.get("port", DEFAULT_SDN_PORT)),
            "type_http": str(sdn_endpoint.get("type_http", DEFAULT_SDN_TYPE_HTTP)),
        }
        if out["type_http"] not in ("http", "https"):
            raise ValueError(f"type_http must be http or https, got {out['type_http']!r}")
        if out["port"] < 1:
            raise ValueError(f"sdn port must be >= 1, got {out['port']}")
        return out
    if isinstance(sdn_endpoint, str):
        raw = sdn_endpoint.strip()
        scheme = DEFAULT_SDN_TYPE_HTTP
        if raw.startswith("http://"):
            scheme = "http"
            raw = raw[len("http://") :]
        elif raw.startswith("https://"):
            scheme = "https"
            raw = raw[len("https://") :]
        if raw.endswith("/"):
            raw = raw[:-1]
        if ":" in raw:
            ip, port_str = raw.rsplit(":", 1)
            try:
                port = int(port_str)
            except ValueError as exc:
                raise ValueError(
                    f"invalid port in sdn_endpoint: {sdn_endpoint!r}"
                ) from exc
        else:
            ip = raw
            port = DEFAULT_SDN_PORT
        if not ip:
            raise ValueError(f"sdn_endpoint missing ip: {sdn_endpoint!r}")
        if port < 1:
            raise ValueError(f"sdn port must be >= 1, got {port}")
        return {"ip": ip, "port": port, "type_http": scheme}
    raise TypeError(
        f"sdn_endpoint must be str/dict/None, got {type(sdn_endpoint).__name__}"
    )


def _validate_topology(topology: dict[str, Any]) -> None:
    if not isinstance(topology, dict):
        raise TypeError(f"topology must be a dict, got {type(topology).__name__}")
    if "nodes" not in topology or "links" not in topology:
        raise ValueError("topology must contain 'nodes' and 'links' keys")
    nodes = topology["nodes"]
    links = topology["links"]
    if not isinstance(nodes, list) or not isinstance(links, list):
        raise ValueError("topology.nodes and topology.links must be lists")
    if not nodes:
        raise ValueError("topology has zero nodes")

    node_uids: set[str] = set()
    for nd in nodes:
        if not isinstance(nd, dict):
            raise ValueError(f"node must be a dict, got {type(nd).__name__}")
        missing = [k for k in REQUIRED_NODE_FIELDS if k not in nd]
        if missing:
            raise ValueError(f"node missing fields {missing}: {nd}")
        uid = nd["uid"]
        if uid in node_uids:
            raise ValueError(f"duplicated node uid: {uid}")
        node_uids.add(uid)

    edge_uids: set[str] = set()
    for ln in links:
        if not isinstance(ln, dict):
            raise ValueError(f"link must be a dict, got {type(ln).__name__}")
        missing = [k for k in REQUIRED_LINK_FIELDS if k not in ln]
        if missing:
            raise ValueError(f"link missing fields {missing}: {ln}")
        if ln["source_uid"] not in node_uids:
            raise ValueError(f"link source_uid not in nodes: {ln['source_uid']}")
        if ln["target_uid"] not in node_uids:
            raise ValueError(f"link target_uid not in nodes: {ln['target_uid']}")
        if ln["source_uid"] == ln["target_uid"]:
            raise ValueError(f"self-loop in link: {ln['uid']}")
        if ln["link_type"] not in ("QKD", "PQC", "HYBRID"):
            raise ValueError(f"invalid link_type: {ln['link_type']}")
        if ln["distance_km"] < 0:
            raise ValueError(f"distance_km < 0: {ln['distance_km']}")
        if ln["quditto_rate_r0"] <= 0:
            raise ValueError(f"quditto_rate_r0 must be > 0: {ln['quditto_rate_r0']}")
        if ln["quditto_rate_alpha"] < 0:
            raise ValueError(
                f"quditto_rate_alpha must be >= 0: {ln['quditto_rate_alpha']}"
            )
        if ln["quditto_max_buffer_size"] < 1:
            raise ValueError(
                f"quditto_max_buffer_size must be >= 1: {ln['quditto_max_buffer_size']}"
            )
        if ln["uid"] in edge_uids:
            raise ValueError(f"duplicated link uid: {ln['uid']}")
        edge_uids.add(ln["uid"])


def _apply_link_overrides(
    links: list[dict[str, Any]],
    r0: float | None,
    alpha: float | None,
    distance_km: int | None,
    buffer_max: int | None,
) -> list[dict[str, Any]]:
    out: list[dict[str, Any]] = []
    for ln in links:
        new_ln = dict(ln)
        if r0 is not None:
            if r0 <= 0:
                raise ValueError(f"r0 must be > 0, got {r0}")
            new_ln["quditto_rate_r0"] = float(r0)
        if alpha is not None:
            if alpha < 0:
                raise ValueError(f"alpha must be >= 0, got {alpha}")
            new_ln["quditto_rate_alpha"] = float(alpha)
        if distance_km is not None:
            if distance_km < 0:
                raise ValueError(f"distance_km must be >= 0, got {distance_km}")
            new_ln["distance_km"] = int(distance_km)
        if buffer_max is not None:
            if buffer_max < 1:
                raise ValueError(f"buffer_max must be >= 1, got {buffer_max}")
            new_ln["quditto_max_buffer_size"] = int(buffer_max)
        out.append(new_ln)
    return out


def build_sim_payload(
    topology: dict[str, Any],
    *,
    name: str,
    description: str | None = None,
    sdn_endpoint: str | dict[str, Any] | None = None,
    owner_uid: int | None = None,
    r0: float | None = None,
    alpha: float | None = None,
    distance_km: int | None = None,
    buffer_max: int | None = None,
) -> dict[str, Any]:
    """Wrap a topology fragment into a full ``WebSimulationUpsertRequest`` body.

    Args:
        topology: Output of a builder in ``topology_builders``. Must
            have ``nodes`` and ``links`` lists with the canonical fields.
        name: Simulation name (required, non-empty).
        description: Optional description. If ``None`` and ``owner_uid``
            is set, a default description is generated.
        sdn_endpoint: Either ``"ip:port"`` (with optional ``http://`` or
            ``https://`` prefix), or a dict ``{"ip", "port", "type_http"}``,
            or ``None`` for the default ``{172.30.0.2, 3000, http}``.
        owner_uid: Optional user id. Not part of the orchestator schema
            (the ``X-User-Id`` header carries it), but used to seed
            ``description`` when none is given.
        r0, alpha, distance_km, buffer_max: Optional per-link overrides
            applied to every link of the topology before wrapping. ``None``
            means "keep the builder's value".

    Returns:
        Dict ready to be sent as the JSON body of
        ``POST /orch/web/simulations``.
    """
    if not isinstance(name, str) or not name.strip():
        raise ValueError("name must be a non-empty string")
    name_clean = name.strip()
    _validate_topology(topology)

    links = _apply_link_overrides(
        list(topology["links"]), r0, alpha, distance_km, buffer_max
    )

    desc = description
    if desc is None and owner_uid is not None:
        desc = f"Created by dkms-topo for owner_uid={owner_uid}"

    return {
        "name": name_clean,
        "description": desc,
        "sdn": _normalize_sdn(sdn_endpoint),
        "nodes": [dict(nd) for nd in topology["nodes"]],
        "links": links,
    }


__all__ = [
    "DEFAULT_SDN_IP",
    "DEFAULT_SDN_PORT",
    "DEFAULT_SDN_TYPE_HTTP",
    "REQUIRED_LINK_FIELDS",
    "REQUIRED_NODE_FIELDS",
    "build_sim_payload",
]
