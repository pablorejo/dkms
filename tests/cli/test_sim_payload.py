"""Unit tests for ``tests/cli/sim_payload``."""

from __future__ import annotations

import pytest

from tests.cli.sim_payload import (
    DEFAULT_SDN_IP,
    DEFAULT_SDN_PORT,
    DEFAULT_SDN_TYPE_HTTP,
    build_sim_payload,
)
from tests.cli.topology_builders import build_mesh, build_ring, build_star


# -----------------------------------------------------------------------------
# Defaults
# -----------------------------------------------------------------------------


def test_default_sdn_block() -> None:
    p = build_sim_payload(build_ring(4), name="t")
    assert p["sdn"] == {
        "ip": DEFAULT_SDN_IP,
        "port": DEFAULT_SDN_PORT,
        "type_http": DEFAULT_SDN_TYPE_HTTP,
    }


def test_minimum_payload_has_all_top_level_keys() -> None:
    p = build_sim_payload(build_ring(3), name="basic")
    assert set(p.keys()) == {"name", "description", "sdn", "nodes", "links"}
    assert p["name"] == "basic"
    assert p["description"] is None
    assert len(p["nodes"]) == 3
    assert len(p["links"]) == 3


def test_name_is_stripped() -> None:
    p = build_sim_payload(build_ring(3), name="   spacey   ")
    assert p["name"] == "spacey"


# -----------------------------------------------------------------------------
# SDN normalization
# -----------------------------------------------------------------------------


def test_sdn_endpoint_string_with_port() -> None:
    p = build_sim_payload(
        build_ring(3), name="t", sdn_endpoint="10.0.0.1:5000"
    )
    assert p["sdn"] == {"ip": "10.0.0.1", "port": 5000, "type_http": "http"}


def test_sdn_endpoint_string_with_http_scheme() -> None:
    p = build_sim_payload(
        build_ring(3), name="t", sdn_endpoint="http://sdn.local:3000"
    )
    assert p["sdn"] == {"ip": "sdn.local", "port": 3000, "type_http": "http"}


def test_sdn_endpoint_string_with_https_scheme() -> None:
    p = build_sim_payload(
        build_ring(3), name="t", sdn_endpoint="https://sdn.x.io:8443"
    )
    assert p["sdn"] == {"ip": "sdn.x.io", "port": 8443, "type_http": "https"}


def test_sdn_endpoint_string_trailing_slash() -> None:
    p = build_sim_payload(
        build_ring(3), name="t", sdn_endpoint="http://sdn:3000/"
    )
    assert p["sdn"]["ip"] == "sdn"
    assert p["sdn"]["port"] == 3000


def test_sdn_endpoint_string_without_port_uses_default() -> None:
    p = build_sim_payload(build_ring(3), name="t", sdn_endpoint="sdn.local")
    assert p["sdn"]["ip"] == "sdn.local"
    assert p["sdn"]["port"] == DEFAULT_SDN_PORT


def test_sdn_endpoint_dict_passthrough() -> None:
    p = build_sim_payload(
        build_ring(3),
        name="t",
        sdn_endpoint={"ip": "9.9.9.9", "port": 4242, "type_http": "https"},
    )
    assert p["sdn"] == {"ip": "9.9.9.9", "port": 4242, "type_http": "https"}


def test_sdn_endpoint_dict_partial_fills_defaults() -> None:
    p = build_sim_payload(build_ring(3), name="t", sdn_endpoint={"port": 6000})
    assert p["sdn"]["port"] == 6000
    assert p["sdn"]["ip"] == DEFAULT_SDN_IP
    assert p["sdn"]["type_http"] == DEFAULT_SDN_TYPE_HTTP


def test_sdn_endpoint_invalid_port_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", sdn_endpoint="sdn:notanint")


def test_sdn_endpoint_invalid_scheme_in_dict() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(
            build_ring(3),
            name="t",
            sdn_endpoint={"ip": "x", "port": 1, "type_http": "ftp"},
        )


def test_sdn_endpoint_unsupported_type() -> None:
    with pytest.raises(TypeError):
        build_sim_payload(build_ring(3), name="t", sdn_endpoint=123)  # type: ignore[arg-type]


# -----------------------------------------------------------------------------
# Link overrides
# -----------------------------------------------------------------------------


def test_link_overrides_applied_to_all_links() -> None:
    p = build_sim_payload(
        build_ring(4),
        name="t",
        r0=4321.0,
        alpha=0.5,
        distance_km=10,
        buffer_max=1024,
    )
    for ln in p["links"]:
        assert ln["quditto_rate_r0"] == 4321.0
        assert ln["quditto_rate_alpha"] == 0.5
        assert ln["distance_km"] == 10
        assert ln["quditto_max_buffer_size"] == 1024


def test_overrides_do_not_mutate_input_topology() -> None:
    topo = build_ring(4)
    original_r0 = topo["links"][0]["quditto_rate_r0"]
    build_sim_payload(topo, name="t", r0=9999.0)
    # Caller's topology stays intact
    assert topo["links"][0]["quditto_rate_r0"] == original_r0


def test_override_r0_invalid_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", r0=0)
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", r0=-1)


def test_override_alpha_negative_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", alpha=-0.1)


def test_override_distance_negative_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", distance_km=-1)


def test_override_buffer_too_small_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="t", buffer_max=0)


# -----------------------------------------------------------------------------
# Description / owner_uid
# -----------------------------------------------------------------------------


def test_owner_uid_seeds_default_description() -> None:
    p = build_sim_payload(build_ring(3), name="t", owner_uid=42)
    assert p["description"] is not None
    assert "42" in p["description"]


def test_explicit_description_overrides_owner_uid() -> None:
    p = build_sim_payload(
        build_ring(3), name="t", description="my desc", owner_uid=42
    )
    assert p["description"] == "my desc"


def test_no_owner_no_description() -> None:
    p = build_sim_payload(build_ring(3), name="t")
    assert p["description"] is None


# -----------------------------------------------------------------------------
# Topology validation
# -----------------------------------------------------------------------------


def test_invalid_name_empty_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="")
    with pytest.raises(ValueError):
        build_sim_payload(build_ring(3), name="   ")


def test_missing_nodes_or_links_raises() -> None:
    with pytest.raises(ValueError):
        build_sim_payload({"nodes": [], "links": []}, name="t")
    with pytest.raises(ValueError):
        build_sim_payload({"nodes": []}, name="t")  # type: ignore[arg-type]


def test_non_dict_topology_raises() -> None:
    with pytest.raises(TypeError):
        build_sim_payload("nope", name="t")  # type: ignore[arg-type]


def test_orphan_link_endpoint_raises() -> None:
    bad = {
        "nodes": [
            {"uid": "a", "node_id": 1, "label": "a", "x": 0.0, "y": 0.0},
        ],
        "links": [
            {
                "uid": "e",
                "source_uid": "a",
                "target_uid": "ghost",
                "link_type": "QKD",
                "distance_km": 0,
                "quditto_rate_r0": 1.0,
                "quditto_rate_alpha": 0.0,
                "quditto_max_buffer_size": 1,
            }
        ],
    }
    with pytest.raises(ValueError):
        build_sim_payload(bad, name="t")


def test_self_loop_raises() -> None:
    bad = {
        "nodes": [
            {"uid": "a", "node_id": 1, "label": "a", "x": 0.0, "y": 0.0},
            {"uid": "b", "node_id": 2, "label": "b", "x": 1.0, "y": 1.0},
        ],
        "links": [
            {
                "uid": "e",
                "source_uid": "a",
                "target_uid": "a",
                "link_type": "QKD",
                "distance_km": 0,
                "quditto_rate_r0": 1.0,
                "quditto_rate_alpha": 0.0,
                "quditto_max_buffer_size": 1,
            }
        ],
    }
    with pytest.raises(ValueError):
        build_sim_payload(bad, name="t")


# -----------------------------------------------------------------------------
# Wider topology coverage
# -----------------------------------------------------------------------------


def test_star_payload_structure() -> None:
    p = build_sim_payload(build_star(2, 3), name="star-2x3")
    assert len(p["nodes"]) == 1 + 2 * 3
    assert len(p["links"]) == 2 * 3


def test_mesh_payload_structure() -> None:
    p = build_sim_payload(build_mesh(3, 3), name="mesh-3x3")
    assert len(p["nodes"]) == 9
    assert len(p["links"]) == 12
