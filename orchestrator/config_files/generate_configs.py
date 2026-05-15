#!/usr/bin/env python3
"""Generate DKMS/ORR/QKC config files from ``config/topology.json``."""
from __future__ import annotations

from pathlib import Path
from typing import Dict, Iterable, Tuple
from ipaddress import IPv4Address, ip_network
import argparse
import json
import sys

ROOT = Path(__file__).resolve().parents[2]
SCRIPTS_DIR = ROOT / "scripts"
if str(SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPTS_DIR))

SUBNET_IP = "127.0.0.0"

from topology_utils import (
    DEFAULT_ETSI,
    DEFAULT_HTTP_TYPE,
    DEFAULT_SIMULATION_ID,
    DEFAULT_TLS_CIPHERS,
    DEFAULT_TLS_VERSION,
    HOST_ROLE_DKMS,
    HOST_ROLE_ORR,
    HOST_ROLE_QKC,
    HOST_ROLE_SDN,
    HOST_ROLE_AGENT_CONTROLLER,
    NODE_IP_BASE,
    Topology,
    TopologyError,
    SAEConfig,
    build_host_payload,
    cleanup_config_dir,
    config_json_path,
    certificate_paths_for,
    dkms_certificate_bundle,
    dkms_id,
    dkms_port,
    load_topology,
    node_label,
    node_service_ip,
    orr_id,
    orr_port,
    qkd_node_id,
    qkc_id,
    qkc_port,
    qkc_url,
)

CONFIG_ENTITY_DIRS = ("DKMS", "ORR", "QKC", "AgentControllers")
SAE_DIR = "SAE"
SDN_DIR = "SDN"
AGENT_CONTROLLER_DIR = "AgentControllers"
SDN_VIRTUAL_NODE_ID = 10_000
SDN_SERVICE_ID = 1
AGENT_CONTROLLER_PORT = 8080


def _infer_subnet(topology: Topology) -> str:
    """
    Determina la subred /24 a partir del topology.json, priorizando la IP de la SDN.
    Si no se puede inferir, utiliza la base de IPs definida en topology_utils.
    """
    # try:
    #     if topology.sdn and topology.sdn.ip:
    #         network = ip_network(f"{topology.sdn.ip}/24", strict=False)
    #         return str(network.network_address)
    # except Exception:
    #     pass
    return f"{SUBNET_IP}"


def ip_for_node(node_id: int, subnet_ip: str) -> str:
    """Return the IPv4 address assigned to a DKMS node."""
    if node_id < 1:
        raise ValueError(f"El identificador de nodo debe ser positivo, recibido: {node_id}")
    base = int(IPv4Address(subnet_ip))
    try:
        addr = IPv4Address(base + 100 + node_id)
    except ValueError as exc:
        raise ValueError(f"No se pudo asignar IP para el nodo {node_id}: {exc}") from exc
    return str(addr)


def ensure_dir(path: Path) -> None:
    path.mkdir(parents=True, exist_ok=True)


def cleanup_legacy_layout(path: Path) -> None:
    """Elimina ficheros con el formato antiguo (p. ej. QKC_1.json)."""
    for prefix in CONFIG_ENTITY_DIRS:
        for legacy in path.glob(f"{prefix}_*.json"):
            legacy.unlink()
    for legacy in path.glob("SAE_*.json"):
        legacy.unlink()
    legacy_sdn = path / "SDN" / "1.json"
    if legacy_sdn.exists():
        legacy_sdn.unlink()


def build_qkc_payload(
    node_id: int,
    topology: Topology,
    sdn_endpoint: str,
    subnet_ip: str,
    simulation_id: int,
    legacy: bool,
) -> Dict:
    node_ip = ip_for_node(node_id, subnet_ip)
    q_port = qkc_port(node_id)
    host = build_host_payload(node_id, HOST_ROLE_QKC, node_ip, q_port, simulation_id=simulation_id)
    kme_host = node_service_ip(node_id)
    label = node_label(node_id)
    cert_template, key_template = certificate_paths_for(label)
    neighbors: list[Dict[str, object]] = []
    local_qkd_id = qkd_node_id(node_id)
    local_url_qkd = qkc_url(node_id)
    for neighbor in topology.neighbors(node_id):
        neighbor_ip = ip_for_node(neighbor, subnet_ip)
        link_cfg = topology.link_config(node_id, neighbor)
        channel_payload: Dict[str, object] = {
            "type_channel": link_cfg.channel_type,
            "distance": int(link_cfg.distance),
        }
        if link_cfg.ttl is not None:
            channel_payload["ttl"] = int(link_cfg.ttl)
        # Emit Quditto channel params under the names the KME Channel
        # pydantic model reads (``quditto_rate_r0`` et al.) so they round
        # trip through ``seed_db_from_configs.py`` into the DB instead of
        # being replaced by column defaults.
        if link_cfg.rate_r0 is not None:
            channel_payload["quditto_rate_r0"] = float(link_cfg.rate_r0)
        if link_cfg.rate_alpha is not None:
            channel_payload["quditto_rate_alpha"] = float(link_cfg.rate_alpha)
        if link_cfg.max_buffer_size is not None:
            channel_payload["quditto_max_buffer_size"] = int(link_cfg.max_buffer_size)
        kme_entry: Dict[str, object] = {
            "local_qkc_id": node_id,
            "local_qkc_ip": node_ip,
            "neighbor_qkc_id": neighbor,
            "neighbor_qkc_ip": neighbor_ip,
            "neighbor_qkc_port": qkc_port(neighbor),
            "local_qkd_id": local_qkd_id,
            "neighbor_qkd_id": qkd_node_id(neighbor),
            "local_url_node_qkd": local_url_qkd,
            "etsi": DEFAULT_ETSI,
            "channel": channel_payload,
            "pqc_simulation": bool(link_cfg.pqc_simulation),
            "pqc_kme_port": 6000,
            "cert": {"path": cert_template["path"]},
            "key": {"path": key_template["path"]},
        }
        if link_cfg.hybrid_enabled:
            kme_entry["hybrid_enabled"] = True
        neighbors.append(kme_entry)

    payload: Dict[str, object] = {
        "id": node_id,
        "id_host": host["id"],
        "host": host,
        "kme_host": kme_host,
        "kmes": neighbors,
    }
    if legacy:
        payload.update(
            {
                "QKC_id": qkc_id(node_id),
                "host_id": host["id"],
                "ip": node_ip,
                "port": q_port,
            }
        )
    return payload


def determine_sdn_settings(topology: Topology) -> Tuple[str, int]:
    if topology.sdn is None:
        raise TopologyError("La topología no define la sección 'sdn'")
    return topology.sdn.ip, topology.sdn.port


def build_orr_payload(
    node_id: int,
    sdn_endpoint: str,
    subnet_ip: str,
    simulation_id: int,
    legacy: bool,
) -> Dict:
    node_ip = ip_for_node(node_id, subnet_ip)
    o_port = orr_port(node_id)
    host = build_host_payload(node_id, HOST_ROLE_ORR, node_ip, o_port, simulation_id=simulation_id)
    payload: Dict[str, object] = {
        "id": node_id,
        "id_host": host["id"],
        "host": host,
        "qkc_id": node_id,
    }
    if legacy:
        payload.update(
            {
                "ORR_id": orr_id(node_id),
                "host_id": host["id"],
                "QKC_id": qkc_id(node_id),
                "ip": node_ip,
                "port": o_port,
            }
        )
    return payload


def build_dkms_payload(
    node_id: int,
    orr_payload: Dict[str, object],
    subnet_ip: str,
    simulation_id: int,
    legacy: bool,
) -> Dict:
    node_ip = ip_for_node(node_id, subnet_ip)
    d_port = dkms_port(node_id)
    host = build_host_payload(node_id, HOST_ROLE_DKMS, node_ip, d_port, simulation_id=simulation_id)
    tls_bundle = dkms_certificate_bundle(node_id)
    payload: Dict[str, object] = {
        "id": node_id,
        "id_host": host["id"],
        "host": host,
        "orr_id": orr_payload["id"],
        "tls": {
            **tls_bundle,
            "require_client_cert": False,
            "version": DEFAULT_TLS_VERSION,
            "ciphers": list(DEFAULT_TLS_CIPHERS),
        },
    }

    orr_summary = {
        "id": orr_payload["id"],
        "id_host": orr_payload["id_host"],
        "qkc_id": orr_payload["qkc_id"],
    }
    host_data = orr_payload.get("host")
    if isinstance(host_data, dict):
        orr_summary["host"] = dict(host_data)
    payload["orr"] = orr_summary

    if legacy:
        payload.update(
            {
                "DKMS_id": dkms_id(node_id),
                "ORR_id": orr_id(node_id),
                "host_id": host["id"],
                "ip": node_ip,
                "port": d_port,
            }
        )
        agent_host = build_host_payload(
            node_id,
            HOST_ROLE_AGENT_CONTROLLER,
            node_ip,
            AGENT_CONTROLLER_PORT,
            simulation_id=simulation_id,
        )
        payload["agent_controllers"] = [
            {
                "id": node_id,
                "id_dkms": node_id,
                "id_sdn": SDN_SERVICE_ID,
                "id_host": agent_host["id"],
                "host": agent_host,
            }
        ]
    return payload


def build_agent_controller_payload(
    node_id: int,
    subnet_ip: str,
    simulation_id: int,
) -> Dict[str, object]:
    node_ip = ip_for_node(node_id, subnet_ip)
    host = build_host_payload(
        node_id,
        HOST_ROLE_AGENT_CONTROLLER,
        node_ip,
        AGENT_CONTROLLER_PORT,
        simulation_id=simulation_id,
    )
    return {
        "id": node_id,
        "id_dkms": node_id,
        "id_sdn": SDN_SERVICE_ID,
        "id_host": host["id"],
        "host": host,
    }


def build_sdn_payload(
    sdn_ip: str,
    sdn_port: int,
    simulation_id: int,
    legacy: bool,
) -> Dict[str, object]:
    host = build_host_payload(
        SDN_VIRTUAL_NODE_ID,
        HOST_ROLE_SDN,
        sdn_ip,
        sdn_port,
        simulation_id=simulation_id,
    )
    payload: Dict[str, object] = {
        "id": SDN_SERVICE_ID,
        "id_host": host["id"],
        "host": host,
        "type_http": DEFAULT_HTTP_TYPE,
    }
    if legacy:
        payload.update({"ip": sdn_ip, "port": sdn_port})
    return payload


def sae_tls_bundle(sae_name: str, dkms_node: int) -> Dict[str, str]:
    base = f"code_dkms/src/certificates/DKMSs/{dkms_id(dkms_node)}/{sae_name}"
    return {
        "ca_certs": {"path": f"code_dkms/src/certificates/DKMSs/{dkms_id(dkms_node)}/rootCA.pem"},
        "use_client_cert": True,
        "cert": {"path": f"{base}/dkms-client.pem"},
        "key": {"path": f"{base}/dkms-client-key.pem"},
    }


def build_sae_payload(
    sae_entry: SAEConfig,
    sdn_ip: str,
    sdn_port: int,
    dkms_payload: Dict[str, object],
) -> Dict:
    host_info = dkms_payload.get("host") or {}
    raw_ip = host_info.get("ip") or dkms_payload.get("ip")
    raw_port = host_info.get("port") or dkms_payload.get("port")
    if raw_ip is None or raw_port is None:
        raise ValueError(f"DKMS {dkms_payload.get('id')} carece de ip/puerto")
    target_ip = str(raw_ip)
    target_port = int(raw_port)
    tls = sae_tls_bundle(sae_entry.name, sae_entry.dkms)

    if sae_entry.ip and sae_entry.ip != target_ip:
        print(
            f"[warning] SAE {sae_entry.name}: ignoring topology ip {sae_entry.ip} in favor of DKMS {sae_entry.dkms} ({target_ip})."
        )

    if sae_entry.port and sae_entry.port != target_port:
        print(
            f"[warning] SAE {sae_entry.name}: ignoring topology port {sae_entry.port} in favor of DKMS {sae_entry.dkms} ({target_port})."
        )

    return {
        "id": sae_entry.name,
        "dkms_target": {
            "ip": target_ip,
            "port": target_port,
        },
        "sdn": {
            "ip": sdn_ip,
            "port": sdn_port,
        },
        "tls": tls,
    }


def write_json(path: Path, payload: Dict) -> None:
    path.write_text(json.dumps(payload, indent=4) + "\n", encoding="utf-8")


def parse_args() -> argparse.Namespace:
    default_topology = ROOT / "config" / "topology.json"
    parser = argparse.ArgumentParser(description="Genera configuraciones DKMS/ORR/QKC/SAE desde topology.json")
    parser.add_argument("--topology", type=Path, default=default_topology, help="Ruta al topology.json")
    parser.add_argument(
        "--output-dir",
        type=Path,
        default=Path(__file__).resolve().parent,
        help="Directorio donde escribir los ficheros JSON",
    )
    parser.add_argument(
        "--simulation-id",
        type=int,
        default=DEFAULT_SIMULATION_ID,
        help="Identificador de la simulación para los hosts generados",
    )
    parser.add_argument(
        "--legacy",
        action="store_true",
        help="Incluye campos legacy (QKC_id, ip/port, etc.) para compatibilidad",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        topology = load_topology(args.topology)
        sdn_ip, sdn_port = determine_sdn_settings(topology)
        subnet_ip = _infer_subnet(topology)
    except TopologyError as exc:
        print(f"[ERROR] {exc}")
        return 1

    ensure_dir(args.output_dir)
    cleanup_legacy_layout(args.output_dir)
    node_ids = set(topology.nodes)
    sdn_endpoint = f"{sdn_ip}:{sdn_port}"
    sae_entries: Iterable[SAEConfig] = list(getattr(topology, "saes", []))
    legacy_mode = bool(args.legacy)
    simulation_id = int(args.simulation_id)

    # Clean up stale files grouped by entity type.
    for folder in CONFIG_ENTITY_DIRS:
        cleanup_config_dir(args.output_dir, folder, node_ids)
    cleanup_config_dir(args.output_dir, SAE_DIR, [entry.name for entry in sae_entries])

    dkms_payloads: Dict[int, Dict[str, object]] = {}
    for node_id in topology.nodes:
        qkc_payload = build_qkc_payload(
            node_id,
            topology,
            sdn_endpoint,
            subnet_ip,
            simulation_id,
            legacy_mode,
        )
        write_json(config_json_path(args.output_dir, "QKC", node_id), qkc_payload)

        orr_payload = build_orr_payload(
            node_id,
            sdn_endpoint,
            subnet_ip,
            simulation_id,
            legacy_mode,
        )
        write_json(config_json_path(args.output_dir, "ORR", node_id), orr_payload)

        payload = build_dkms_payload(
            node_id,
            orr_payload,
            subnet_ip,
            simulation_id,
            legacy_mode,
        )
        write_json(config_json_path(args.output_dir, "DKMS", node_id), payload)
        if isinstance(node_id, int):
            dkms_payloads[node_id] = payload

        agent_payload = build_agent_controller_payload(node_id, subnet_ip, simulation_id)
        write_json(
            config_json_path(args.output_dir, AGENT_CONTROLLER_DIR, node_id),
            agent_payload,
        )

    write_json(
        config_json_path(args.output_dir, SDN_DIR, SDN_SERVICE_ID),
        build_sdn_payload(sdn_ip, sdn_port, simulation_id, legacy_mode),
    )

    count_saes = 0
    for sae_entry in sae_entries:
        dkms_payload = dkms_payloads.get(sae_entry.dkms)
        if dkms_payload is None:
            print(f"[warning] SAE {sae_entry.name}: DKMS {sae_entry.dkms} not defined; skipping.")
            continue
        write_json(
            config_json_path(args.output_dir, SAE_DIR, sae_entry.name),
            build_sae_payload(sae_entry, sdn_ip, sdn_port, dkms_payload),
        )
        count_saes += 1

    print(
        f"Generados {len(topology.nodes)} nodos (QKC/ORR/DKMS) y {count_saes} SAE(s) en {args.output_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
