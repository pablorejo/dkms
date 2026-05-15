#!/usr/bin/env python3
"""Utility helpers to derive deployment artifacts from ``config/topology.json``.

This module exposes a light abstraction over the project topology so both
CLI generators (YAML + JSON) and shell scripts can share the same logic.
The topology file is expected to be a JSON document with a ``connections``
array of ``{"init": <int>, "end": <int>, "type": "QKD|PQC"}`` objects
(the ``type`` field is optional) and, optionally, a ``nodes`` array for
isolated nodes. Duplicate or reversed connections are coalesced into
undirected edges.
"""
from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Set, Tuple, Union
import json

# Default constants reused across generators. Adjust here if the addressing
# scheme ever needs to change.
QKC_PORT_BASE = 5000
ORR_PORT_BASE = 3000
DKMS_PORT_BASE = 4000
SDN_PORT = 3000
SITE_PREFIX = "site_"
NODE_IP_BASE = "172.30.0."
NODE_IP_OFFSET = 200
LOCALHOST = "127.0.0.1"
DEFAULT_PROTOCOL = "bb84"
DEFAULT_ETSI = "ETSI_014"
DEFAULT_TLS_VERSION = "TLSv1_2"
DEFAULT_TLS_CIPHERS = [
    "TLS_AES_256_GCM_SHA384",
    "TLS_CHACHA20_POLY1305_SHA256",
    "TLS_AES_128_GCM_SHA256"
]
DEFAULT_HTTP_TYPE = "http"
DEFAULT_SIMULATION_ID = 1
CHANNEL_QKD = "qkd"
CHANNEL_PQC_SIMULATION = "pqc-simulation"

HOST_ROLE_QKC = "qkc"
HOST_ROLE_ORR = "orr"
HOST_ROLE_DKMS = "dkms"
HOST_ROLE_SDN = "sdn"
HOST_ROLE_AGENT_CONTROLLER = "agent_controller"

_HOST_ROLE_OFFSETS = {
    HOST_ROLE_QKC: 1,
    HOST_ROLE_ORR: 2,
    HOST_ROLE_DKMS: 3,
    HOST_ROLE_SDN: 4,
    HOST_ROLE_AGENT_CONTROLLER: 5,
}
_HOST_ID_FACTOR = 10


# Los parámetros anteriores centralizan el plan de direccionamiento y cifrado
# para que los generadores y scripts dependientes compartan los mismos valores.


@dataclass(frozen=True)
class SDNConfig:
    """Minimal configuration required to run the SDN service."""

    ip: str
    port: int


@dataclass(frozen=True)
class SAEConfig:
    """Description of an SAE attachment present in topology.json."""

    name: str
    dkms: int
    ip: Optional[str] = None
    port: Optional[int] = None


@dataclass(frozen=True)
class LinkConfig:
    """Optional per-link settings parsed from topology connections."""

    channel_type: str = CHANNEL_QKD
    distance: int = 0
    pqc_simulation: bool = False
    ttl: Optional[int] = None
    rate_r0: Optional[float] = None
    rate_alpha: Optional[float] = None
    max_buffer_size: Optional[int] = None
    hybrid_enabled: bool = False


@dataclass(frozen=True)
class Topology:
    """Minimal undirected graph representation for the topology."""

    nodes: List[int]
    adjacency: Dict[int, Set[int]]
    saes: List[SAEConfig] = field(default_factory=list)
    sdn: Optional[SDNConfig] = None
    dkms_positions: Dict[int, Dict[str, float]] = field(default_factory=dict)
    sae_positions: Dict[str, Dict[str, float]] = field(default_factory=dict)
    link_configs: Dict[Tuple[int, int], LinkConfig] = field(default_factory=dict)

    def neighbors(self, node_id: int) -> List[int]:
        # Ordenar garantiza resultados deterministas al iterar vecinos.
        return sorted(self.adjacency.get(node_id, set()))

    def link_config(self, node_a: int, node_b: int) -> LinkConfig:
        key = (min(int(node_a), int(node_b)), max(int(node_a), int(node_b)))
        return self.link_configs.get(key, LinkConfig())


class TopologyError(RuntimeError):
    pass


def _normalize_connection(entry: object) -> Optional[Tuple[int, int]]:
    if not isinstance(entry, dict):
        return None
    try:
        a = int(entry["init"])
        b = int(entry["end"])
    except Exception as exc:  # KeyError/ValueError/TypeError
        raise TopologyError(f"Entrada de conexión inválida: {entry!r}") from exc
    if a == b:
        # Ignore self-loops; they do not contribute to adjacency
        return None
    # La tupla original se ordenará posteriormente, evitando duplicados y
    # diferencias por conexión invertida.
    return (a, b)


def _parse_bool(value: object, default: bool = False) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return bool(value)
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in {"1", "true", "yes", "y", "on"}:
            return True
        if lowered in {"0", "false", "no", "n", "off"}:
            return False
    return default


def _normalize_channel_type(value: object) -> str:
    if value is None:
        return CHANNEL_QKD
    raw = str(value).strip().lower().replace("_", "-")
    if raw in {"qkd"}:
        return CHANNEL_QKD
    if raw in {"pqc-simulation", "pqc", "simulated", "simulation"}:
        return CHANNEL_PQC_SIMULATION
    return CHANNEL_QKD


def _parse_link_config(entry: object) -> LinkConfig:
    if not isinstance(entry, dict):
        return LinkConfig()

    channel_payload = entry.get("channel")
    channel_type_raw = None
    distance_raw = None
    ttl_raw = None
    rate_r0_raw: object = None
    rate_alpha_raw: object = None
    max_buffer_size_raw: object = None
    if isinstance(channel_payload, dict):
        channel_type_raw = channel_payload.get("type_channel") or channel_payload.get("type")
        distance_raw = channel_payload.get("distance")
        ttl_raw = channel_payload.get("ttl")
        rate_r0_raw = (
            channel_payload.get("rate_r0")
            or channel_payload.get("quditto_rate_r0")
        )
        rate_alpha_raw = (
            channel_payload.get("rate_alpha")
            or channel_payload.get("quditto_rate_alpha")
        )
        max_buffer_size_raw = (
            channel_payload.get("max_buffer_size")
            or channel_payload.get("quditto_max_buffer_size")
        )

    if channel_type_raw is None:
        channel_type_raw = entry.get("type_channel") or entry.get("channel_type") or entry.get("type")
    if distance_raw is None:
        distance_raw = entry.get("distance")
    if ttl_raw is None:
        ttl_raw = entry.get("ttl")

    channel_type = _normalize_channel_type(channel_type_raw)
    pqc_simulation = _parse_bool(entry.get("pqc_simulation"), default=False) or channel_type == CHANNEL_PQC_SIMULATION
    hybrid_enabled = _parse_bool(entry.get("hybrid_enabled"), default=False)

    try:
        distance = int(distance_raw) if distance_raw is not None else 0
    except (TypeError, ValueError):
        distance = 0
    if distance < 0:
        distance = 0

    ttl: Optional[int]
    try:
        ttl = int(ttl_raw) if ttl_raw is not None else None
    except (TypeError, ValueError):
        ttl = None
    if ttl is not None and ttl <= 0:
        ttl = None

    def _maybe_float(value: object) -> Optional[float]:
        if value is None:
            return None
        try:
            parsed = float(value)
        except (TypeError, ValueError):
            return None
        return parsed if parsed > 0 else None

    rate_r0 = _maybe_float(rate_r0_raw)
    rate_alpha_val = _maybe_float(rate_alpha_raw)
    rate_alpha: Optional[float]
    if rate_alpha_raw is None:
        rate_alpha = None
    else:
        try:
            rate_alpha_cast = float(rate_alpha_raw)
        except (TypeError, ValueError):
            rate_alpha = None
        else:
            rate_alpha = rate_alpha_cast if rate_alpha_cast >= 0 else None
    # keep rate_alpha_val referenced for static analysers (no-op)
    del rate_alpha_val

    max_buffer_size: Optional[int]
    try:
        max_buffer_size = int(max_buffer_size_raw) if max_buffer_size_raw is not None else None
    except (TypeError, ValueError):
        max_buffer_size = None
    if max_buffer_size is not None and max_buffer_size <= 0:
        max_buffer_size = None

    return LinkConfig(
        channel_type=channel_type,
        distance=distance,
        pqc_simulation=pqc_simulation,
        ttl=ttl,
        rate_r0=rate_r0,
        rate_alpha=rate_alpha,
        max_buffer_size=max_buffer_size,
        hybrid_enabled=hybrid_enabled,
    )


def load_topology(path: Path) -> Topology:
    """Load ``config/topology.json`` and build an undirected graph."""
    if not path.exists():
        raise TopologyError(f"No se encontró el fichero de topología: {path}")
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as exc:
        raise TopologyError(f"topology.json inválido: {exc}") from exc

    nodes: Set[int] = set()
    adjacency: Dict[int, Set[int]] = {}

    # Optional explicit node list
    explicit_nodes = raw.get("nodes") if isinstance(raw, dict) else None
    if isinstance(explicit_nodes, Iterable):
        for item in explicit_nodes:
            if isinstance(item, dict):
                node_id = item.get("id")
                if isinstance(node_id, int):
                    nodes.add(node_id)
                continue
            try:
                nodes.add(int(item))
            except (TypeError, ValueError):
                # Permitir entradas no numéricas (p. ej. SAE) en el listado
                continue

    connections = raw.get("connections") if isinstance(raw, dict) else None
    if not isinstance(connections, Iterable):
        connections = []

    edges: Set[Tuple[int, int]] = set()
    link_configs: Dict[Tuple[int, int], LinkConfig] = {}
    for item in connections:
        conn = _normalize_connection(item)
        if conn is None:
            continue
        a, b = conn
        nodes.update({a, b})
        edge = (min(a, b), max(a, b))
        edges.add(edge)
        link_configs[edge] = _parse_link_config(item)

    for node_id in nodes:
        adjacency.setdefault(node_id, set())

    for a, b in edges:
        adjacency.setdefault(a, set()).add(b)
        adjacency.setdefault(b, set()).add(a)

    raw_saes = raw.get("saes") if isinstance(raw, dict) else None
    saes: List[SAEConfig] = []
    if isinstance(raw_saes, Iterable):
        for entry in raw_saes:
            if not isinstance(entry, dict):
                continue
            name = entry.get("name")
            dkms_ref = entry.get("dkms")
            if not name or dkms_ref is None:
                raise TopologyError(f"Entrada SAE incompleta: {entry!r}")
            try:
                dkms_id_int = int(dkms_ref)
            except (TypeError, ValueError) as exc:
                raise TopologyError(f"Identificador DKMS inválido en SAE: {entry!r}") from exc
            nodes.add(dkms_id_int)
            ip_val = entry.get("ip")
            ip_str = str(ip_val) if ip_val is not None else None
            port_val = entry.get("port")
            port_int: Optional[int]
            if port_val is None:
                port_int = None
            else:
                try:
                    port_int = int(port_val)
                except (TypeError, ValueError) as exc:
                    raise TopologyError(f"Puerto inválido en SAE: {entry!r}") from exc
            saes.append(SAEConfig(name=str(name), dkms=dkms_id_int, ip=ip_str, port=port_int))

    raw_sdn = raw.get("sdn") if isinstance(raw, dict) else None
    sdn_cfg: Optional[SDNConfig] = None
    if isinstance(raw_sdn, dict):
        ip_val = raw_sdn.get("ip")
        port_val = raw_sdn.get("port")
        if ip_val is None or port_val is None:
            raise TopologyError(f"Entrada SDN incompleta: {raw_sdn!r}")
        try:
            port_int = int(port_val)
        except (TypeError, ValueError) as exc:
            raise TopologyError(f"Puerto inválido en SDN: {raw_sdn!r}") from exc
        sdn_cfg = SDNConfig(ip=str(ip_val), port=port_int)

    raw_layout = raw.get("nodes") if isinstance(raw, dict) else None
    dkms_positions: Dict[int, Dict[str, float]] = {}
    sae_positions: Dict[str, Dict[str, float]] = {}
    if isinstance(raw_layout, Iterable):
        for entry in raw_layout:
            if not isinstance(entry, dict):
                continue
            x_val = entry.get("x")
            y_val = entry.get("y")
            if not isinstance(x_val, (int, float)) or not isinstance(y_val, (int, float)):
                continue
            node_type = entry.get("type")
            if node_type == "sae":
                node_id_raw = entry.get("id")
                if isinstance(node_id_raw, str) and node_id_raw:
                    sae_positions[node_id_raw] = {"x": float(x_val), "y": float(y_val)}
            else:
                node_id_raw = entry.get("id")
                try:
                    node_id_int = int(node_id_raw)
                except (TypeError, ValueError):
                    continue
                dkms_positions[node_id_int] = {"x": float(x_val), "y": float(y_val)}

    if not nodes:
        raise TopologyError("La topología no define ningún nodo")

    return Topology(
        nodes=sorted(nodes),
        adjacency=adjacency,
        saes=saes,
        sdn=sdn_cfg,
        dkms_positions=dkms_positions,
        sae_positions=sae_positions,
        link_configs=link_configs,
    )


def node_label(node_id: int) -> str:
    """Alphabetic label (A, B, ..., Z, AA, AB, ...)."""
    if node_id <= 0:
        raise ValueError(f"El identificador de nodo debe ser positivo, recibido {node_id}")
    label = ""
    n = node_id
    while n > 0:
        n -= 1
        label = chr(ord('A') + (n % 26)) + label
        n //= 26
    return label


def qkd_node_id(node_id: int) -> str:
    """Identificador lógico del nodo QKD asociado al QKC."""
    return node_label(node_id)


def node_site(node_id: int) -> str:
    return f"{SITE_PREFIX}{node_id}"


def node_service_ip(node_id: int) -> str:
    return f"{NODE_IP_BASE}{NODE_IP_OFFSET + node_id}"


def host_id_for(node_id: int, role: str) -> int:
    """Compute a stable host identifier for a given node/role pair."""
    if node_id <= 0:
        raise ValueError(f"El identificador de nodo debe ser positivo, recibido {node_id}")
    role_key = role.lower()
    try:
        offset = _HOST_ROLE_OFFSETS[role_key]
    except KeyError as exc:  # pragma: no cover - valid roles están fijados arriba
        raise ValueError(f"Rol de host desconocido: {role}") from exc
    return node_id * _HOST_ID_FACTOR + offset


def build_host_payload(
    node_id: int,
    role: str,
    ip: str,
    port: int,
    simulation_id: int = DEFAULT_SIMULATION_ID,
) -> Dict[str, int | str]:
    """Genera el diccionario compatible con ModelHost utilizado en los JSON."""
    return {
        "id": host_id_for(node_id, role),
        "id_simulation": simulation_id,
        "ip": ip,
        "port": port,
    }


def ensure_config_dir(base_dir: Path, name: str) -> Path:
    """Crea (si es necesario) la carpeta donde se guardan los JSON de un tipo."""
    target = base_dir / name
    target.mkdir(parents=True, exist_ok=True)
    return target


def cleanup_config_dir(base_dir: Path, name: str, valid_ids: Iterable[Union[int, str]]) -> None:
    """Elimina configuraciones antiguas dentro de la carpeta indicada."""
    directory = ensure_config_dir(base_dir, name)
    valid = {str(identifier) for identifier in valid_ids}
    for config_file in directory.glob("*.json"):
        if not valid or config_file.stem not in valid:
            config_file.unlink()


def config_json_path(base_dir: Path, name: str, identifier: Union[int, str]) -> Path:
    """Devuelve la ruta ``<base>/<name>/<identifier>.json`` asegurando la carpeta."""
    directory = ensure_config_dir(base_dir, name)
    return directory / f"{identifier}.json"


def qkc_id(node_id: int) -> str:
    return f"QKC_{node_id}"


def orr_id(node_id: int) -> str:
    return f"ORR_{node_id}"


def dkms_id(node_id: int) -> str:
    return f"DKMS_{node_id}"


def qkc_port(node_id: int) -> int:
    return QKC_PORT_BASE + node_id


def orr_port(node_id: int) -> int:
    return ORR_PORT_BASE + node_id


def dkms_port(node_id: int) -> int:
    return DKMS_PORT_BASE + node_id


def qkc_url(node_id: int) -> str:
    # El nodo QKD (QuDitto) se despliega como sidecar en el mismo pod DKMS.
    return "https://127.0.0.1:5000"


def certificate_paths_for(label: str) -> Tuple[str, str]:
    """Return certificate and key relative paths for a logical node label."""
    base = "code_dkms/src/certificates"
    certificate = {"path": f"{base}/{label}/client{label}.pem"}
    key = {"path": f"{base}/{label}/client{label}.key"}
    return certificate, key


def dkms_certificate_bundle(node_id: int) -> Dict[str, str]:
    base = f"code_dkms/src/certificates/DKMSs/{dkms_id(node_id)}"
    return {
        "cert": {"path": f"{base}/cert.pem"} ,
        "key":  {"path": f"{base}/key.pem"},
        "ca_cert": {"path": f"{base}/rootCA.pem"},
    }
