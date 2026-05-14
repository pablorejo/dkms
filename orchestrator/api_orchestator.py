from __future__ import annotations

import json
import os
import secrets
import socket
import tempfile
from collections import deque
from contextlib import contextmanager
from datetime import datetime, timezone
from math import cos, pi, sin
from typing import Any, Dict, List, Literal, Optional
from urllib import error as urllib_error
from urllib import request as urllib_request
from urllib.parse import urlparse

from fastapi import Body, Depends, FastAPI, Header, HTTPException, Query, status
from fastapi.responses import JSONResponse, Response
from pydantic import AliasChoices, BaseModel, Field, ValidationError, field_validator, model_validator
from sqlalchemy.exc import IntegrityError, OperationalError

from models import (
    Channel,
    ChannelType,
    ETSIType,
    HTTPType,
    KMEConfig,
    ModelDKMS,
    ModelFile,
    ModelHost,
    ModelORR,
    ModelQKC,
    ModelSAE,
    ModelSDN,
    ModelSimulation,
    SaeStatus,
    SimulationStatus,
)
from persistence import build_uow_from_env
from sae_certificates import (
    bundle_to_pkcs12_base64,
    issue_sae_certificate,
    sign_sae_csr,
)
try:
    from k8s.runtime_ca import get_or_create_runtime_ca_material
except ModuleNotFoundError:  # pragma: no cover - fallback for script execution
    from runtime_ca import get_or_create_runtime_ca_material

try:
    from k8s.orchestator import Orchestator
except ModuleNotFoundError:  # pragma: no cover - fallback for script execution
    from orchestator import Orchestator

try:
    from k8s.pods import (
        PodLoadtest,
        DOCKER_HUB_SECRET_NAME,
        K8S_INGRESS_HOST,
        K8S_RUNTIME_INGRESS_HOST,
    )
except ModuleNotFoundError:  # pragma: no cover
    from pods import (  # type: ignore
        PodLoadtest,
        DOCKER_HUB_SECRET_NAME,
        K8S_INGRESS_HOST,
        K8S_RUNTIME_INGRESS_HOST,
    )

DEFAULT_QUDITTO_RATE_R0 = 2000.0
DEFAULT_QUDITTO_RATE_ALPHA = 0.2


class SimulationActionResponse(BaseModel):
    status: str
    simulation_id: int
    action: str


class LoadTestCreateRequest(BaseModel):
    start_saes: int = Field(default=5, ge=0, le=5000)
    end_saes: int = Field(default=50, ge=1, le=5000)
    step_saes: int = Field(default=5, ge=1, le=5000)
    interval_seconds: float = Field(default=15.0, ge=1.0, le=3600.0)
    offset_seconds: Optional[float] = Field(default=None, ge=0.0, le=7200.0)
    # Tiempo con n_saes=0 antes de arrancar la rampa. Da margen a quditto
    # para llenar los buffers KME→peer; sin esto, los primeros SAEs pegan
    # contra un token bucket en fill_rate=0 y ven 429s en cascada.
    warmup_seconds: float = Field(default=30.0, ge=0.0, le=3600.0)
    key_size_bits: int = Field(default=256, ge=64, le=4096)
    per_sae_lambda: float = Field(default=0.5, ge=0.05, le=50.0)
    request_timeout_seconds: int = Field(default=60, ge=5, le=600)

    @model_validator(mode="after")
    def _check_ramp(self) -> "LoadTestCreateRequest":
        if self.end_saes < self.start_saes:
            raise ValueError("end_saes must be >= start_saes")
        return self


class LoadTestInfo(BaseModel):
    test_id: str
    deployment_name: str
    simulation_id: int
    grafana_url: str
    replicas: int = 0
    ready_replicas: int = 0
    available_replicas: int = 0
    created_at: Optional[str] = None


class WebSDNConfig(BaseModel):
    ip: str = "172.30.0.2"
    port: int = Field(default=3000, ge=1)
    type_http: Literal["http", "https"] = Field(
        default="http",
        validation_alias=AliasChoices("type_http", "typeHttp"),
    )


class WebNodeInput(BaseModel):
    uid: str
    node_id: int = Field(ge=1, validation_alias=AliasChoices("node_id", "nodeId"))
    label: str
    x: float
    y: float
    enc_buffer_size: Optional[int] = Field(
        default=None,
        ge=0,
        validation_alias=AliasChoices("enc_buffer_size", "encBufferSize"),
    )
    dec_buffer_size: Optional[int] = Field(
        default=None,
        ge=0,
        validation_alias=AliasChoices("dec_buffer_size", "decBufferSize"),
    )


class WebLinkInput(BaseModel):
    uid: str
    source_uid: str = Field(validation_alias=AliasChoices("source_uid", "sourceUid"))
    target_uid: str = Field(validation_alias=AliasChoices("target_uid", "targetUid"))
    link_type: Literal["QKD", "PQC", "HYBRID"] = Field(
        validation_alias=AliasChoices("link_type", "linkType")
    )
    distance_km: int = Field(
        default=0,
        ge=0,
        validation_alias=AliasChoices("distance_km", "distanceKm"),
    )
    quditto_max_buffer_size: int = Field(
        default=100,
        ge=1,
        validation_alias=AliasChoices("quditto_max_buffer_size", "qudittoMaxBufferSize"),
    )
    quditto_rate_r0: float = Field(
        default=DEFAULT_QUDITTO_RATE_R0,
        gt=0,
        validation_alias=AliasChoices("quditto_rate_r0", "qudittoRateR0", "rate_r0", "r0"),
    )
    quditto_rate_alpha: float = Field(
        default=DEFAULT_QUDITTO_RATE_ALPHA,
        ge=0,
        validation_alias=AliasChoices("quditto_rate_alpha", "qudittoRateAlpha", "rate_alpha", "alpha"),
    )

    @field_validator("link_type", mode="before")
    @classmethod
    def _normalize_link_type(cls, value: object) -> str:
        text = str(value or "QKD").strip().upper().replace("_", "-")
        if text in {"PQC", "PQC-SIMULATION", "SIMULATED", "SIMULATION", "SIMUL"}:
            return "PQC"
        if text in {"HYBRID", "QKD+PQC", "QKD-PQC"}:
            return "HYBRID"
        if text in {"QKD", "CLASSIC", "NORMAL", "REAL", "DIRECT"}:
            return "QKD"
        return "QKD"

    @model_validator(mode="after")
    def _normalize_pqc_params(self) -> "WebLinkInput":
        if self.link_type == "PQC":
            self.distance_km = 0
            self.quditto_max_buffer_size = 10000
            self.quditto_rate_r0 = DEFAULT_QUDITTO_RATE_R0
            self.quditto_rate_alpha = DEFAULT_QUDITTO_RATE_ALPHA
        return self


class WebSimulationUpsertRequest(BaseModel):
    name: str
    description: Optional[str] = None
    sdn: WebSDNConfig = Field(default_factory=WebSDNConfig)
    nodes: List[WebNodeInput] = Field(default_factory=list)
    links: List[WebLinkInput] = Field(default_factory=list)


class WebSimulationDTO(BaseModel):
    id: int
    name: str
    description: Optional[str] = None
    status: str
    sdn: WebSDNConfig
    created_at: str
    updated_at: str
    nodes: List[WebNodeInput]
    links: List[WebLinkInput]


class WebSimulationSummaryDTO(BaseModel):
    id: int
    name: str
    description: Optional[str] = None
    status: str
    created_at: str
    updated_at: str
    node_count: int
    link_count: int


class WebSimulationRunDTO(BaseModel):
    id: int
    simulation_id: int
    status: Literal["QUEUED", "RUNNING", "DONE", "FAILED"]
    message: Optional[str] = None
    queued_at: Optional[str] = None
    started_at: Optional[str] = None
    finished_at: Optional[str] = None
    created_at: str


class SaeAdminCreateRequest(BaseModel):
    simulation_id: int = Field(ge=1)
    dkms_id: int = Field(ge=1)
    sae_id: str = Field(min_length=1, max_length=255)
    display_name: Optional[str] = Field(default=None, max_length=255)

    @field_validator("sae_id")
    @classmethod
    def _normalize_sae_id(cls, value: str) -> str:
        normalized = str(value or "").strip()
        if not normalized:
            raise ValueError("sae_id is required")
        return normalized


class SaeIssueCSRRequest(BaseModel):
    csr_pem: str = Field(min_length=1)
    days_valid: int = Field(default=90, ge=1, le=825)


class SaeIssueRequest(BaseModel):
    key_type: Literal["ec-p256", "rsa-2048"] = "ec-p256"
    days_valid: int = Field(default=90, ge=1, le=825)
    bundle_format: Literal["pem", "pkcs12"] = "pem"
    pkcs12_password: Optional[str] = None


class SaeRevokeRequest(BaseModel):
    reason: Optional[str] = Field(default=None, max_length=255)


class SaeAdminDTO(BaseModel):
    id: int
    sae_id: str
    display_name: Optional[str] = None
    owner_user_id: Optional[int] = None
    simulation_id: Optional[int] = None
    dkms_id: Optional[int] = None
    status: Literal["pending_cert", "active", "revoked", "expired"]
    cert_serial: Optional[str] = None
    cert_fingerprint: Optional[str] = None
    cert_subject: Optional[str] = None
    cert_not_before: Optional[str] = None
    cert_not_after: Optional[str] = None
    revoked_at: Optional[str] = None
    revocation_reason: Optional[str] = None
    created_at: Optional[str] = None
    updated_at: Optional[str] = None


class SaeIssueResponse(BaseModel):
    sae: SaeAdminDTO
    certificate_pem: str
    ca_chain_pem: str
    private_key_pem: Optional[str] = None
    bundle_pkcs12_base64: Optional[str] = None


class SaeBundleResponse(BaseModel):
    sae: SaeAdminDTO
    format: Literal["pem", "pkcs12"]
    certificate_pem: Optional[str] = None
    ca_chain_pem: Optional[str] = None
    private_key_pem: Optional[str] = None
    bundle_pkcs12_base64: Optional[str] = None


class SaeDeleteResponse(BaseModel):
    status: Literal["deleted"]
    sae_id: str


class SDNDKMSTargetPayload(BaseModel):
    ip: str = Field(min_length=1)
    port: int = Field(ge=1)


class SDNSAECreatePayload(BaseModel):
    id: str = Field(min_length=1, max_length=255)
    dkms_id: Optional[str] = None
    dkms_target: Optional[SDNDKMSTargetPayload] = None

    @field_validator("id")
    @classmethod
    def _normalize_id(cls, value: str) -> str:
        normalized = str(value or "").strip()
        if not normalized:
            raise ValueError("id is required")
        return normalized

    @model_validator(mode="after")
    def _ensure_selector(self) -> "SDNSAECreatePayload":
        if not self.dkms_id and self.dkms_target is None:
            raise ValueError("Debe especificarse dkms_id o dkms_target")
        return self


class SDNSAEUpdatePayload(BaseModel):
    dkms_id: Optional[str] = None
    dkms_target: Optional[SDNDKMSTargetPayload] = None

    @model_validator(mode="after")
    def _ensure_selector(self) -> "SDNSAEUpdatePayload":
        if not self.dkms_id and self.dkms_target is None:
            raise ValueError("Debe especificarse dkms_id o dkms_target")
        return self


class SDNHostDTO(BaseModel):
    id: int
    ip: str
    port: int


class SDNQKCDTO(BaseModel):
    id: int
    host: SDNHostDTO
    kme_host: Optional[str] = None


class SDNORRDTO(BaseModel):
    id: int
    host: SDNHostDTO
    qkc: SDNQKCDTO


class SDNDKMSDTO(BaseModel):
    id: int
    host: SDNHostDTO
    tls_id: Optional[int] = None
    orr: SDNORRDTO


class SDNSAEDTO(BaseModel):
    id: str
    dkms: SDNDKMSDTO


class SDNSAEBindingDTO(BaseModel):
    sae_id: str
    dkms_id: str
    dkms_endpoint: dict[str, Any]
    orr_id: str


SDN_SYNC_TIMEOUT_SECONDS = max(
    1.0,
    float(os.getenv("ORCH_SDN_SYNC_TIMEOUT_SECONDS", "60")),
)


def _require_user_id(x_user_id: Optional[str] = Header(default=None, alias="X-User-Id")) -> int:
    if not x_user_id:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Missing X-User-Id header (request must go through AuthZ ingress)",
        )
    try:
        return int(x_user_id)
    except (TypeError, ValueError) as exc:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Invalid X-User-Id header",
        ) from exc


def _validate_sim_header(simulation_id: int, x_simulation_id: Optional[str]) -> None:
    if not x_simulation_id:
        return
    try:
        header_simulation_id = int(x_simulation_id)
    except (TypeError, ValueError) as exc:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Invalid X-Simulation-Id header",
        ) from exc
    if header_simulation_id != simulation_id:
        raise HTTPException(
            status_code=status.HTTP_403_FORBIDDEN,
            detail=(
                "X-Simulation-Id header does not match requested simulation id"
            ),
        )


def _build_orchestator() -> Orchestator:
    return Orchestator(uow=build_uow_from_env())


def _value_error_to_http(exc: ValueError) -> HTTPException:
    detail = str(exc)
    if "no encontrada" in detail.lower() or "not found" in detail.lower():
        return HTTPException(status_code=status.HTTP_404_NOT_FOUND, detail=detail)
    if "no pertenece al usuario" in detail.lower() or "forbidden" in detail.lower():
        return HTTPException(status_code=status.HTTP_403_FORBIDDEN, detail=detail)
    return HTTPException(status_code=status.HTTP_400_BAD_REQUEST, detail=detail)


def _to_iso(value: Optional[datetime]) -> str:
    if value is None:
        return datetime.now(timezone.utc).isoformat()
    return value.isoformat()


def _normalize_http_type(raw: str) -> HTTPType:
    # NOTE: current enum names are inverted in models/enums.py.
    return HTTPType.HTTPS if raw == "http" else HTTPType.HTTP


def _http_type_to_text(value: Optional[HTTPType]) -> str:
    if value is None:
        return "http"
    raw = getattr(value, "value", str(value))
    return "https" if raw == "https" else "http"


def _node_ip(node_id: int) -> str:
    return f"127.0.0.{100 + int(node_id)}"


def _qkc_port(node_id: int) -> int:
    return 5000 + int(node_id)


def _orr_port(node_id: int) -> int:
    return 3000 + int(node_id)


def _dkms_port(node_id: int) -> int:
    return 4000 + int(node_id)


def _node_label(node_id: int) -> str:
    return f"DKMS-{int(node_id)}"


def _kme_material_base_dir() -> str:
    override = str(os.getenv("ORCH_KME_MATERIAL_DIR", "")).strip()
    if override:
        return override
    return tempfile.gettempdir()


def _kme_material_paths_for_node(node_id: int) -> tuple[str, str]:
    # Paths are deterministic within the node scope and include a random suffix.
    suffix = secrets.token_hex(16)
    base_name = f"dkms-{int(node_id)}-{suffix}"
    base_dir = _kme_material_base_dir()
    cert_path = os.path.join(base_dir, f"{base_name}.crt")
    key_path = os.path.join(base_dir, f"{base_name}.key")
    return cert_path, key_path


def _link_qkd_distance(link: WebLinkInput) -> int:
    return max(0, int(getattr(link, "distance_km", 0) or 0))


def _link_qkd_buffer(link: WebLinkInput) -> int:
    return max(1, int(getattr(link, "quditto_max_buffer_size", 10000) or 10000))


def _normalize_quditto_rate_r0_value(raw_value: Any) -> float:
    try:
        value = float(raw_value)
    except (TypeError, ValueError):
        value = DEFAULT_QUDITTO_RATE_R0
    return value if value > 0 else DEFAULT_QUDITTO_RATE_R0


def _normalize_quditto_rate_alpha_value(raw_value: Any) -> float:
    try:
        value = float(raw_value)
    except (TypeError, ValueError):
        value = DEFAULT_QUDITTO_RATE_ALPHA
    return value if value >= 0 else DEFAULT_QUDITTO_RATE_ALPHA


def _link_qkd_rate_r0(link: WebLinkInput) -> float:
    return _normalize_quditto_rate_r0_value(
        getattr(link, "quditto_rate_r0", DEFAULT_QUDITTO_RATE_R0)
    )


def _link_qkd_rate_alpha(link: WebLinkInput) -> float:
    return _normalize_quditto_rate_alpha_value(
        getattr(link, "quditto_rate_alpha", DEFAULT_QUDITTO_RATE_ALPHA)
    )


def _topology_json_from_request(payload: WebSimulationUpsertRequest) -> str:
    blob = {
        "name": payload.name,
        "description": payload.description,
        "sdn": payload.sdn.model_dump(),
        "nodes": [node.model_dump() for node in payload.nodes],
        "links": [link.model_dump() for link in payload.links],
    }
    return json.dumps(blob, ensure_ascii=True, sort_keys=True)


def _validate_web_topology(payload: WebSimulationUpsertRequest) -> None:
    if not payload.name.strip():
        raise HTTPException(status_code=400, detail="Simulation name is required")

    node_uid_set: set[str] = set()
    node_id_set: set[int] = set()
    for node in payload.nodes:
        if node.uid in node_uid_set:
            raise HTTPException(status_code=400, detail=f"Duplicate node uid: {node.uid}")
        node_uid_set.add(node.uid)
        if node.node_id in node_id_set:
            raise HTTPException(status_code=400, detail=f"Duplicate node_id: {node.node_id}")
        node_id_set.add(node.node_id)

    link_uid_set: set[str] = set()
    link_pair_set: set[tuple[str, str]] = set()
    for link in payload.links:
        if link.uid in link_uid_set:
            raise HTTPException(status_code=400, detail=f"Duplicate link uid: {link.uid}")
        link_uid_set.add(link.uid)
        if link.source_uid == link.target_uid:
            raise HTTPException(status_code=400, detail="Self-links are not allowed")
        if link.source_uid not in node_uid_set or link.target_uid not in node_uid_set:
            raise HTTPException(
                status_code=400,
                detail=f"Link {link.uid} references unknown nodes",
            )
        pair_key = _edge_pair_key(link.source_uid, link.target_uid)
        if pair_key in link_pair_set:
            raise HTTPException(
                status_code=400,
                detail=f"Duplicate undirected link between nodes {pair_key[0]} and {pair_key[1]}",
            )
        link_pair_set.add(pair_key)


def _build_model_simulation_from_web(
    user_id: int,
    payload: WebSimulationUpsertRequest,
) -> ModelSimulation:
    _validate_web_topology(payload)

    node_by_uid: dict[str, WebNodeInput] = {node.uid: node for node in payload.nodes}
    qkc_id_by_uid: dict[str, int] = {
        node.uid: 100000 + int(node.node_id)
        for node in payload.nodes
    }

    adjacency: dict[str, list[WebLinkInput]] = {node.uid: [] for node in payload.nodes}
    dedup: set[tuple[str, str]] = set()
    for link in payload.links:
        left = min(link.source_uid, link.target_uid)
        right = max(link.source_uid, link.target_uid)
        key = (left, right)
        if key in dedup:
            continue
        dedup.add(key)
        adjacency[link.source_uid].append(link)
        adjacency[link.target_uid].append(link)

    dkms_models: list[ModelDKMS] = []

    for node in sorted(payload.nodes, key=lambda item: item.node_id):
        node_id = int(node.node_id)
        local_uid = node.uid
        local_ip = _node_ip(node_id)
        local_qkc_id = qkc_id_by_uid[local_uid]
        cert_path, key_path = _kme_material_paths_for_node(node_id)

        kmes: list[KMEConfig] = []
        for link in adjacency.get(local_uid, []):
            neighbor_uid = link.target_uid if link.source_uid == local_uid else link.source_uid
            neighbor_node = node_by_uid[neighbor_uid]
            neighbor_id = int(neighbor_node.node_id)
            is_pqc = link.link_type == "PQC"
            is_hybrid = link.link_type == "HYBRID"
            channel_type = ChannelType.PQC_SIMULATION if is_pqc else ChannelType.QKD
            kmes.append(
                KMEConfig(
                    id=None,
                    local_qkc_id=local_qkc_id,
                    local_qkc_ip=local_ip,
                    neighbor_qkc_id=qkc_id_by_uid[neighbor_uid],
                    neighbor_qkc_ip=_node_ip(neighbor_id),
                    neighbor_qkc_port=_qkc_port(neighbor_id),
                    neighbor_qkd_id=_node_label(neighbor_id),
                    local_qkd_id=_node_label(node_id),
                    local_url_node_qkd="https://127.0.0.1:5000",
                    etsi=ETSIType.ETSI_014,
                    cert=ModelFile(path=cert_path),
                    key=ModelFile(path=key_path),
                    channel=Channel(
                        type_channel=channel_type,
                        distance=0 if is_pqc else _link_qkd_distance(link),
                        quditto_max_buffer_size=10000 if is_pqc else _link_qkd_buffer(link),
                        quditto_rate_r0=DEFAULT_QUDITTO_RATE_R0 if is_pqc else _link_qkd_rate_r0(link),
                        quditto_rate_alpha=DEFAULT_QUDITTO_RATE_ALPHA if is_pqc else _link_qkd_rate_alpha(link),
                    ),
                    pqc_simulation=is_pqc,
                    hybrid_enabled=is_hybrid,
                    pqc_kme_port=6000,
                )
            )

        qkc_model = ModelQKC(
            id=local_qkc_id,
            id_host=None,
            kme_host=f"172.30.0.{200 + node_id}",
            host=ModelHost(id_simulation=0, ip=local_ip, port=_qkc_port(node_id)),
            kmes=kmes,
        )

        orr_model = ModelORR(
            id=None,
            id_host=None,
            qkc_id=local_qkc_id,
            host=ModelHost(id_simulation=0, ip=local_ip, port=_orr_port(node_id)),
            qkc=qkc_model,
        )

        dkms_models.append(
            ModelDKMS(
                id=None,
                id_host=None,
                orr_id=local_qkc_id,
                host=ModelHost(id_simulation=0, ip=local_ip, port=_dkms_port(node_id)),
                orr=orr_model,
                tls=None,
            )
        )

    sdn_model = ModelSDN(
        id=None,
        id_host=None,
        host=ModelHost(id_simulation=0, ip=payload.sdn.ip, port=int(payload.sdn.port)),
        type_http=_normalize_http_type(payload.sdn.type_http),
    )

    return ModelSimulation(
        id=None,
        id_user=int(user_id),
        name=payload.name.strip(),
        description=payload.description,
        status=SimulationStatus.PENDING,
        list_dkms=dkms_models,
        sdn=sdn_model,
    )


def _simulation_status_text(value: Optional[SimulationStatus]) -> str:
    raw = getattr(value, "value", "pending")
    return str(raw)


def _edge_style_to_link_type(raw_channel: str, *, hybrid_enabled: bool = False) -> Literal["QKD", "PQC", "HYBRID"]:
    if hybrid_enabled:
        return "HYBRID"
    if raw_channel == "pqc-simulation":
        return "PQC"
    return "QKD"


def _reconstruct_editor_topology(simulation: ModelSimulation) -> tuple[list[WebNodeInput], list[WebLinkInput], WebSDNConfig]:
    dkms_sorted = sorted(simulation.list_dkms, key=lambda node: int(node.id or 0))
    count = max(len(dkms_sorted), 1)

    node_uid_by_qkc: dict[int, str] = {}
    nodes: list[WebNodeInput] = []

    for index, dkms in enumerate(dkms_sorted):
        node_id = int(dkms.id or (index + 1))
        uid = f"node-{node_id}"
        angle = (2 * pi * index) / count
        nodes.append(
            WebNodeInput(
                uid=uid,
                node_id=node_id,
                label=f"DKMS {node_id}",
                x=320 + 220 * cos(angle),
                y=220 + 160 * sin(angle),
                enc_buffer_size=None,
                dec_buffer_size=None,
            )
        )
        qkc_id = int(getattr(getattr(dkms.orr, "qkc", None), "id", 0) or 0)
        if qkc_id > 0:
            node_uid_by_qkc[qkc_id] = uid

    edges: list[WebLinkInput] = []
    edge_pairs: set[tuple[str, str]] = set()

    for dkms in dkms_sorted:
        local_qkc = getattr(getattr(dkms.orr, "qkc", None), "id", None)
        if local_qkc is None:
            continue
        source_uid = node_uid_by_qkc.get(int(local_qkc))
        if not source_uid:
            continue
        for kme in getattr(getattr(dkms.orr, "qkc", None), "kmes", []) or []:
            neighbor_uid = node_uid_by_qkc.get(int(kme.neighbor_qkc_id))
            if not neighbor_uid or neighbor_uid == source_uid:
                continue
            left = min(source_uid, neighbor_uid)
            right = max(source_uid, neighbor_uid)
            if (left, right) in edge_pairs:
                continue
            edge_pairs.add((left, right))
            channel = getattr(kme.channel, "type_channel", ChannelType.QKD)
            channel_raw = getattr(channel, "value", str(channel))
            is_pqc = channel_raw == ChannelType.PQC_SIMULATION.value or bool(getattr(kme, "pqc_simulation", False))
            is_hybrid = bool(getattr(kme, "hybrid_enabled", False))
            edges.append(
                WebLinkInput(
                    uid=f"edge-{left}-{right}",
                    source_uid=left,
                    target_uid=right,
                    link_type=_edge_style_to_link_type(channel_raw, hybrid_enabled=is_hybrid),
                    distance_km=0 if is_pqc else max(0, int(getattr(getattr(kme, "channel", None), "distance", 0) or 0)),
                    quditto_max_buffer_size=(
                        100
                        if is_pqc
                        else max(
                            1,
                            int(
                                getattr(
                                    getattr(kme, "channel", None),
                                    "quditto_max_buffer_size",
                                    100,
                                )
                                or 100
                            ),
                        )
                    ),
                    quditto_rate_r0=(
                        DEFAULT_QUDITTO_RATE_R0
                        if is_pqc
                        else _normalize_quditto_rate_r0_value(
                            getattr(
                                getattr(kme, "channel", None),
                                "quditto_rate_r0",
                                DEFAULT_QUDITTO_RATE_R0,
                            )
                        )
                    ),
                    quditto_rate_alpha=(
                        DEFAULT_QUDITTO_RATE_ALPHA
                        if is_pqc
                        else _normalize_quditto_rate_alpha_value(
                            getattr(
                                getattr(kme, "channel", None),
                                "quditto_rate_alpha",
                                DEFAULT_QUDITTO_RATE_ALPHA,
                            )
                        )
                    ),
                )
            )

    sdn_host = simulation.sdn.host if simulation.sdn else None
    sdn = WebSDNConfig(
        ip=str(getattr(sdn_host, "ip", "172.30.0.2")),
        port=int(getattr(sdn_host, "port", 3000) or 3000),
        type_http=_http_type_to_text(getattr(simulation.sdn, "type_http", None)),
    )
    return nodes, edges, sdn


def _simulation_to_web_dto(simulation: ModelSimulation) -> WebSimulationDTO:
    topology_raw = getattr(simulation, "editor_topology_json", None)
    parsed: Optional[dict] = None
    if topology_raw:
        try:
            parsed = json.loads(topology_raw)
        except Exception:  # noqa: BLE001
            parsed = None

    if isinstance(parsed, dict):
        parsed_nodes = parsed.get("nodes")
        parsed_links = parsed.get("links")
        parsed_sdn = parsed.get("sdn")

        nodes: list[WebNodeInput] = []
        links: list[WebLinkInput] = []

        if isinstance(parsed_nodes, list):
            for item in parsed_nodes:
                if not isinstance(item, dict):
                    continue
                try:
                    nodes.append(WebNodeInput.model_validate(item))
                except ValidationError:
                    continue

        if isinstance(parsed_links, list):
            for item in parsed_links:
                if not isinstance(item, dict):
                    continue
                try:
                    links.append(WebLinkInput.model_validate(item))
                except ValidationError:
                    continue

        try:
            sdn = WebSDNConfig.model_validate(parsed_sdn or {})
        except ValidationError:
            sdn = WebSDNConfig()
    else:
        nodes, links, sdn = _reconstruct_editor_topology(simulation)

    return WebSimulationDTO(
        id=int(simulation.id or 0),
        name=simulation.name,
        description=simulation.description,
        status=_simulation_status_text(simulation.status),
        sdn=sdn,
        created_at=_to_iso(getattr(simulation, "created_at", None) or simulation.start_time),
        updated_at=_to_iso(getattr(simulation, "updated_at", None) or simulation.start_time),
        nodes=nodes,
        links=links,
    )


def _summary_from_web_dto(simulation: WebSimulationDTO) -> WebSimulationSummaryDTO:
    return WebSimulationSummaryDTO(
        id=simulation.id,
        name=simulation.name,
        description=simulation.description,
        status=simulation.status,
        created_at=simulation.created_at,
        updated_at=simulation.updated_at,
        node_count=len(simulation.nodes),
        link_count=len(simulation.links),
    )


def _is_connected(nodes: list[WebNodeInput], links: list[WebLinkInput]) -> bool:
    if len(nodes) <= 1:
        return True

    adjacency: dict[str, set[str]] = {node.uid: set() for node in nodes}
    for link in links:
        if link.source_uid == link.target_uid:
            continue
        if link.source_uid not in adjacency or link.target_uid not in adjacency:
            continue
        adjacency[link.source_uid].add(link.target_uid)
        adjacency[link.target_uid].add(link.source_uid)

    start_uid = nodes[0].uid
    visited: set[str] = set()
    queue: deque[str] = deque([start_uid])

    while queue:
        current = queue.popleft()
        if current in visited:
            continue
        visited.add(current)
        for neighbor in adjacency.get(current, set()):
            if neighbor not in visited:
                queue.append(neighbor)

    return len(visited) == len(nodes)


@contextmanager
def _sqlalchemy_uow_session():
    uow = build_uow_from_env()
    with uow:
        session = getattr(uow, "_session", None)
        if session is None:
            raise HTTPException(
                status_code=500,
                detail="This endpoint requires sqlalchemy persistence backend",
            )
        yield uow, session


def _to_iso_optional(value: Optional[datetime]) -> Optional[str]:
    if value is None:
        return None
    return value.isoformat()


def _sae_status_text(raw_status: object) -> str:
    raw = str(getattr(raw_status, "value", raw_status) or "").strip().lower()
    if raw in {"pending_cert", "active", "revoked", "expired"}:
        return raw
    return "pending_cert"


def _sae_entity_to_dto(entity) -> SaeAdminDTO:
    sae_id_value = str(getattr(entity, "sae_id", "") or "").strip()
    if not sae_id_value:
        sae_id_value = f"sae-{int(getattr(entity, 'id') or 0)}"
    return SaeAdminDTO(
        id=int(entity.id),
        sae_id=sae_id_value,
        display_name=getattr(entity, "display_name", None),
        owner_user_id=getattr(entity, "owner_user_id", None),
        simulation_id=getattr(entity, "simulation_id", None),
        dkms_id=getattr(entity, "dkms_id", None),
        status=_sae_status_text(getattr(entity, "status", None)),
        cert_serial=getattr(entity, "cert_serial", None),
        cert_fingerprint=getattr(entity, "cert_fingerprint", None),
        cert_subject=getattr(entity, "cert_subject", None),
        cert_not_before=_to_iso_optional(getattr(entity, "cert_not_before", None)),
        cert_not_after=_to_iso_optional(getattr(entity, "cert_not_after", None)),
        revoked_at=_to_iso_optional(getattr(entity, "revoked_at", None)),
        revocation_reason=getattr(entity, "revocation_reason", None),
        created_at=_to_iso_optional(getattr(entity, "created_at", None)),
        updated_at=_to_iso_optional(getattr(entity, "updated_at", None)),
    )


def _require_owned_simulation(session, *, user_id: int, simulation_id: int):
    from persistence.sqlalchemy.data import Simulation as SimulationEntity

    simulation = session.get(SimulationEntity, int(simulation_id))
    if simulation is None:
        raise HTTPException(status_code=404, detail="Simulation not found")
    if int(simulation.id_user) != int(user_id):
        raise HTTPException(status_code=403, detail="Simulation does not belong to authenticated user")
    return simulation


def _resolve_dkms_for_simulation(
    session,
    *,
    simulation_id: int,
    dkms_selector: int,
):
    from persistence.sqlalchemy.data import DKMS as DKMSEntity
    from persistence.sqlalchemy.data import Host as HostEntity

    selector = int(dkms_selector)

    # 1) Match directo por id de la tabla dkms.
    direct = session.get(DKMSEntity, selector)
    if direct is not None:
        direct_host = session.get(HostEntity, int(direct.id_host)) if direct.id_host is not None else None
        if direct_host is not None and int(direct_host.id_simulation) == int(simulation_id):
            return direct

    # 2) Resolver por host de la simulación (útil cuando el frontend usa node_id lógico).
    candidates = (
        session.query(DKMSEntity)
        .join(HostEntity, DKMSEntity.id_host == HostEntity.id)
        .filter(HostEntity.id_simulation == int(simulation_id))
        .all()
    )
    if not candidates:
        raise HTTPException(status_code=404, detail="No DKMS found for simulation")

    # Prioridad: host.id exacto, derivación por puerto 400X, derivación por IP 127.0.0.10X.
    by_host_id = []
    by_port = []
    by_ip = []
    for candidate in candidates:
        host = session.get(HostEntity, int(candidate.id_host)) if candidate.id_host is not None else None
        if host is None:
            continue
        if int(host.id) == selector:
            by_host_id.append(candidate)
            continue

        host_port = getattr(host, "port", None)
        try:
            host_port = int(host_port) if host_port is not None else None
        except (TypeError, ValueError):
            host_port = None
        if host_port is not None and host_port > 4000 and int(host_port - 4000) == selector:
            by_port.append(candidate)
            continue

        host_ip = str(getattr(host, "ip", "") or "")
        chunks = host_ip.split(".")
        if len(chunks) == 4:
            try:
                octets = [int(chunk) for chunk in chunks]
            except ValueError:
                octets = []
            if octets and octets[0:3] == [127, 0, 0] and octets[3] >= 101:
                if int(octets[3] - 100) == selector:
                    by_ip.append(candidate)

    for bucket in (by_host_id, by_port, by_ip):
        if len(bucket) == 1:
            return bucket[0]
        if len(bucket) > 1:
            raise HTTPException(
                status_code=409,
                detail=(
                    f"DKMS selector {selector} is ambiguous in simulation {simulation_id}; "
                    f"matches={[int(item.id) for item in bucket]}"
                ),
            )

    raise HTTPException(
        status_code=404,
        detail=(
            f"DKMS not found for selector={selector} in simulation={int(simulation_id)}. "
            "Use runtime DKMS id or host-aligned node id."
        ),
    )


def _require_dkms_in_simulation(session, *, simulation_id: int, dkms_id: int):
    return _resolve_dkms_for_simulation(
        session,
        simulation_id=int(simulation_id),
        dkms_selector=int(dkms_id),
    )


def _parse_dkms_selector(raw_value: object) -> int:
    try:
        selector = int(str(raw_value or "").strip())
    except (TypeError, ValueError) as exc:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="dkms_id must be an integer selector",
        ) from exc
    if selector <= 0:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="dkms_id must be greater than zero",
        )
    return selector


def _resolve_dkms_by_target_in_simulation(
    session,
    *,
    simulation_id: int,
    target: SDNDKMSTargetPayload,
):
    from persistence.sqlalchemy.data import DKMS as DKMSEntity
    from persistence.sqlalchemy.data import Host as HostEntity

    try:
        target_port = int(target.port)
    except (TypeError, ValueError) as exc:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="dkms_target.port must be an integer",
        ) from exc

    matches = (
        session.query(DKMSEntity)
        .join(HostEntity, DKMSEntity.id_host == HostEntity.id)
        .filter(HostEntity.id_simulation == int(simulation_id))
        .filter(HostEntity.ip == str(target.ip))
        .filter(HostEntity.port == target_port)
        .all()
    )
    if not matches:
        raise HTTPException(
            status_code=status.HTTP_404_NOT_FOUND,
            detail=(
                f"DKMS not found in simulation={int(simulation_id)} "
                f"for dkms_target={target.ip}:{target_port}"
            ),
        )
    if len(matches) > 1:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=(
                f"dkms_target={target.ip}:{target_port} is ambiguous "
                f"in simulation={int(simulation_id)}"
            ),
        )
    return matches[0]


def _resolve_dkms_from_sdn_payload(
    session,
    *,
    simulation_id: int,
    dkms_id: Optional[str],
    dkms_target: Optional[SDNDKMSTargetPayload],
):
    if dkms_id:
        selector = _parse_dkms_selector(dkms_id)
        return _resolve_dkms_for_simulation(
            session,
            simulation_id=int(simulation_id),
            dkms_selector=selector,
        )
    if dkms_target is not None:
        return _resolve_dkms_by_target_in_simulation(
            session,
            simulation_id=int(simulation_id),
            target=dkms_target,
        )
    raise HTTPException(
        status_code=status.HTTP_400_BAD_REQUEST,
        detail="Debe especificarse dkms_id o dkms_target",
    )


def _require_owned_sae_in_simulation(
    session,
    *,
    user_id: int,
    simulation_id: int,
    sae_id: str,
):
    return _require_owned_sae(
        session,
        user_id=user_id,
        sae_id=sae_id,
        simulation_id=int(simulation_id),
    )


def _simulation_is_running(simulation_entity) -> bool:
    raw_status = str(getattr(getattr(simulation_entity, "status", None), "value", getattr(simulation_entity, "status", "")) or "")
    return raw_status.strip().lower() == "running"


def _simulation_sdn_service_base_url(simulation_entity) -> str:
    simulation_id = int(getattr(simulation_entity, "id"))
    sdn_entity = getattr(simulation_entity, "sdn", None)
    if sdn_entity is None:
        hosts = getattr(simulation_entity, "hosts", None) or []
        for host_entity in hosts:
            sdn_nodes = getattr(host_entity, "sdn_nodes", None) or []
            if sdn_nodes:
                sdn_entity = sdn_nodes[0]
                break
    if sdn_entity is None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Simulation {simulation_id} has no SDN assigned",
        )
    sdn_host = getattr(sdn_entity, "host", None)
    if sdn_host is None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Simulation {simulation_id} has no SDN host assigned",
        )

    port = getattr(sdn_host, "port", None)
    if port is None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Simulation {simulation_id} has no SDN port assigned",
        )
    try:
        sdn_port = int(port)
    except (TypeError, ValueError) as exc:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Simulation {simulation_id} has invalid SDN port: {port}",
        ) from exc

    sdn_suffix = getattr(sdn_entity, "id_host", None)
    if sdn_suffix is None:
        sdn_suffix = getattr(sdn_entity, "id", None)
    if sdn_suffix is None:
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail=f"Simulation {simulation_id} has invalid SDN identifiers",
        )
    return f"http://sdn-{int(sdn_suffix)}.{simulation_id}.svc.cluster.local:{sdn_port}"


def _sdn_http_json(
    *,
    method: str,
    url: str,
    payload: Optional[dict[str, Any]] = None,
) -> tuple[int, Optional[dict[str, Any]], str]:
    parsed_url = urlparse(url)
    if parsed_url.scheme not in {"http", "https"} or not parsed_url.netloc:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail=f"Invalid SDN backend URL: {url}",
        )

    body: Optional[bytes] = None
    headers: dict[str, str] = {}
    if payload is not None:
        body = json.dumps(payload, ensure_ascii=True).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib_request.Request(
        url=url,
        data=body,
        headers=headers,
        method=method,
    )
    try:
        # URL validada arriba: solo se permite http/https con netloc explícito.
        with urllib_request.urlopen(request, timeout=SDN_SYNC_TIMEOUT_SECONDS) as response:  # nosec B310
            response_body = response.read().decode("utf-8", errors="replace")
            response_payload: Optional[dict[str, Any]]
            try:
                parsed = json.loads(response_body) if response_body else None
            except json.JSONDecodeError:
                parsed = None
            response_payload = parsed if isinstance(parsed, dict) else None
            return int(response.status), response_payload, response_body
    except urllib_error.HTTPError as exc:
        raw_body = exc.read().decode("utf-8", errors="replace")
        parsed_body: Optional[dict[str, Any]]
        try:
            parsed = json.loads(raw_body) if raw_body else None
        except json.JSONDecodeError:
            parsed = None
        parsed_body = parsed if isinstance(parsed, dict) else None
        return int(exc.code), parsed_body, raw_body
    except (urllib_error.URLError, TimeoutError, socket.timeout) as exc:
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail=f"SDN backend unavailable: {exc}",
        ) from exc


def _resolve_sdn_dkms_id(session, *, dkms_db_id: int) -> int:
    """Translate a DKMS database PK into the canonical node ID the SDN uses.

    The SDN identifies nodes by a small integer (1..N) derived from the host IP
    (127.0.0.10X → X) or port (400X → X).  The database PK is usually a much
    larger auto-increment value that the SDN does not know about.
    """
    from persistence.sqlalchemy.data import DKMS as DKMSEntity
    from persistence.sqlalchemy.data import Host as HostEntity

    dkms_entity = session.get(DKMSEntity, int(dkms_db_id))
    if dkms_entity is None:
        return int(dkms_db_id)

    host_entity = (
        session.get(HostEntity, int(dkms_entity.id_host))
        if dkms_entity.id_host is not None
        else None
    )
    if host_entity is None:
        return int(dkms_db_id)

    host_port = getattr(host_entity, "port", None)
    try:
        port_int = int(host_port) if host_port is not None else 0
    except (TypeError, ValueError):
        port_int = 0
    if port_int > 4000:
        return int(port_int - 4000)

    host_ip = str(getattr(host_entity, "ip", "") or "").strip()
    octets = host_ip.split(".")
    if len(octets) == 4:
        try:
            a, b, c, d = (int(o) for o in octets)
        except ValueError:
            a = b = c = d = -1
        if a == 127 and b == 0 and c == 0 and d > 100:
            return int(d - 100)

    return int(dkms_db_id)


def _sync_sae_binding_to_sdn(
    *,
    simulation_entity,
    sae_id: str,
    dkms_id: int,
    session=None,
) -> None:
    sdn_dkms_id = dkms_id
    if session is not None:
        sdn_dkms_id = _resolve_sdn_dkms_id(session, dkms_db_id=dkms_id)
    base_url = _simulation_sdn_service_base_url(simulation_entity)
    post_url = f"{base_url}/sae/"
    post_payload = {"id": str(sae_id), "dkms_id": str(int(sdn_dkms_id))}
    status_code, _, raw_body = _sdn_http_json(method="POST", url=post_url, payload=post_payload)
    if status_code in {200, 201}:
        return
    if status_code == 409:
        put_url = f"{base_url}/sae/{sae_id}"
        put_payload = {"dkms_id": str(int(sdn_dkms_id))}
        update_status, _, update_body = _sdn_http_json(
            method="PUT",
            url=put_url,
            payload=put_payload,
        )
        if update_status == 200:
            return
        raise HTTPException(
            status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
            detail=(
                "SDN SAE upsert failed during update "
                f"(HTTP {update_status}): {update_body or '<empty>'}"
            ),
        )
    raise HTTPException(
        status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
        detail=(
            "SDN SAE upsert failed during create "
            f"(HTTP {status_code}): {raw_body or '<empty>'}"
        ),
    )


def _delete_sae_binding_from_sdn(*, simulation_entity, sae_id: str) -> None:
    base_url = _simulation_sdn_service_base_url(simulation_entity)
    url = f"{base_url}/sae/{sae_id}"
    status_code, _, raw_body = _sdn_http_json(method="DELETE", url=url, payload=None)
    if status_code in {200, 204, 404}:
        return
    raise HTTPException(
        status_code=status.HTTP_503_SERVICE_UNAVAILABLE,
        detail=(
            "SDN SAE delete failed "
            f"(HTTP {status_code}): {raw_body or '<empty>'}"
        ),
    )


def _build_sdn_host_dto(host_entity) -> SDNHostDTO:
    return SDNHostDTO(
        id=int(getattr(host_entity, "id")),
        ip=str(getattr(host_entity, "ip")),
        port=int(getattr(host_entity, "port")),
    )


def _build_sdn_sae_payload(session, *, sae_entity) -> SDNSAEDTO:
    from persistence.sqlalchemy.data import DKMS as DKMSEntity
    from persistence.sqlalchemy.data import Host as HostEntity
    from persistence.sqlalchemy.data import ORR as ORREntity
    from persistence.sqlalchemy.data import QKC as QKCEntity

    if getattr(sae_entity, "dkms_id", None) is None:
        raise HTTPException(status_code=409, detail="SAE has no DKMS binding")

    dkms_entity = session.get(DKMSEntity, int(sae_entity.dkms_id))
    if dkms_entity is None:
        raise HTTPException(status_code=409, detail="SAE DKMS binding points to missing DKMS")

    dkms_host = session.get(HostEntity, int(dkms_entity.id_host)) if dkms_entity.id_host is not None else None
    if dkms_host is None:
        raise HTTPException(status_code=409, detail="SAE DKMS host is missing")

    orr_entity = session.get(ORREntity, int(dkms_entity.orr_id)) if dkms_entity.orr_id is not None else None
    if orr_entity is None:
        raise HTTPException(status_code=409, detail="SAE DKMS ORR is missing")

    orr_host = session.get(HostEntity, int(orr_entity.id_host)) if orr_entity.id_host is not None else None
    if orr_host is None:
        raise HTTPException(status_code=409, detail="SAE DKMS ORR host is missing")

    qkc_entity = session.get(QKCEntity, int(orr_entity.qkc_id)) if orr_entity.qkc_id is not None else None
    if qkc_entity is None:
        raise HTTPException(status_code=409, detail="SAE DKMS QKC is missing")

    qkc_host = session.get(HostEntity, int(qkc_entity.id_host)) if qkc_entity.id_host is not None else None
    if qkc_host is None:
        raise HTTPException(status_code=409, detail="SAE DKMS QKC host is missing")

    return SDNSAEDTO(
        id=str(getattr(sae_entity, "sae_id", "") or ""),
        dkms=SDNDKMSDTO(
            id=int(dkms_entity.id),
            host=_build_sdn_host_dto(dkms_host),
            tls_id=(int(dkms_entity.tls_id) if getattr(dkms_entity, "tls_id", None) is not None else None),
            orr=SDNORRDTO(
                id=int(orr_entity.id),
                host=_build_sdn_host_dto(orr_host),
                qkc=SDNQKCDTO(
                    id=int(qkc_entity.id),
                    host=_build_sdn_host_dto(qkc_host),
                    kme_host=(
                        str(getattr(qkc_entity, "kme_host"))
                        if getattr(qkc_entity, "kme_host", None) is not None
                        else None
                    ),
                ),
            ),
        ),
    )


def _build_sae_binding_payload(session, *, sae_entity) -> SDNSAEBindingDTO:
    from persistence.sqlalchemy.data import DKMS as DKMSEntity
    from persistence.sqlalchemy.data import Host as HostEntity

    if getattr(sae_entity, "dkms_id", None) is None:
        raise HTTPException(status_code=409, detail="SAE has no DKMS binding")
    dkms_entity = session.get(DKMSEntity, int(sae_entity.dkms_id))
    if dkms_entity is None:
        raise HTTPException(status_code=409, detail="SAE DKMS binding points to missing DKMS")
    dkms_host = session.get(HostEntity, int(dkms_entity.id_host)) if dkms_entity.id_host is not None else None
    if dkms_host is None:
        raise HTTPException(status_code=409, detail="SAE DKMS host is missing")
    if getattr(dkms_entity, "orr_id", None) is None:
        raise HTTPException(status_code=409, detail="SAE DKMS has no ORR binding")

    host_id = int(getattr(dkms_host, "id"))
    raw_ip = str(getattr(dkms_host, "ip", ""))
    # In K8s the DKMS is reachable via its service name (dkms-{host_id}),
    # not the legacy IP stored in the database.
    endpoint_ip = f"dkms-{host_id}" if host_id > 0 else raw_ip

    return SDNSAEBindingDTO(
        sae_id=str(getattr(sae_entity, "sae_id", "") or ""),
        dkms_id=str(int(dkms_entity.id)),
        dkms_endpoint={
            "id": host_id,
            "ip": endpoint_ip,
            "port": int(getattr(dkms_host, "port", 0)),
        },
        orr_id=str(int(dkms_entity.orr_id)),
    )


def _resolve_agent_controller_for_dkms(session, *, dkms_id: int):
    from persistence.sqlalchemy.data import AgentController as AgentControllerEntity

    return (
        session.query(AgentControllerEntity)
        .filter(AgentControllerEntity.id_dkms == int(dkms_id))
        .order_by(AgentControllerEntity.id.asc())
        .first()
    )


def _require_owned_sae(
    session,
    *,
    user_id: int,
    sae_id: str,
    simulation_id: Optional[int] = None,
):
    from persistence.sqlalchemy.data import SAE as SAEEntity

    query = session.query(SAEEntity).filter(SAEEntity.sae_id == str(sae_id))
    if simulation_id is not None:
        query = query.filter(SAEEntity.simulation_id == int(simulation_id))

    entities = query.order_by(SAEEntity.id.asc()).all()
    if not entities:
        raise HTTPException(status_code=404, detail="SAE not found")

    owned_entities: list[Any] = []
    ownership_denied = False
    for entity in entities:
        owner_user_id = getattr(entity, "owner_user_id", None)
        if owner_user_id is not None:
            if int(owner_user_id) == int(user_id):
                owned_entities.append(entity)
            else:
                ownership_denied = True
            continue

        entity_simulation_id = getattr(entity, "simulation_id", None)
        if entity_simulation_id is None:
            ownership_denied = True
            continue
        try:
            _require_owned_simulation(session, user_id=user_id, simulation_id=int(entity_simulation_id))
        except HTTPException:
            ownership_denied = True
            continue
        owned_entities.append(entity)

    if not owned_entities:
        if ownership_denied:
            raise HTTPException(status_code=403, detail="SAE does not belong to authenticated user")
        raise HTTPException(status_code=404, detail="SAE not found")

    if len(owned_entities) > 1:
        if simulation_id is not None:
            raise HTTPException(
                status_code=409,
                detail=(
                    f"SAE '{sae_id}' is duplicated inside simulation {int(simulation_id)}; "
                    "resolve database duplicates before continuing"
                ),
            )
        raise HTTPException(
            status_code=409,
            detail=(
                f"SAE '{sae_id}' is ambiguous across simulations for this user; "
                "provide simulation_id"
            ),
        )

    return owned_entities[0]


def _runtime_ca_material_for_sae(*, sae_entity) -> tuple[str, str]:
    simulation_id = getattr(sae_entity, "simulation_id", None)
    if simulation_id is None:
        raise HTTPException(status_code=409, detail="SAE has no simulation_id assigned")
    try:
        return get_or_create_runtime_ca_material(simulation_id=int(simulation_id))
    except HTTPException:
        raise
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(
            status_code=503,
            detail=f"Runtime CA material is unavailable for simulation {simulation_id}: {exc}",
        ) from exc


def _upsert_sae_tls_bundle(
    session,
    *,
    sae_entity,
    certificate_pem: str,
    ca_chain_pem: str,
    private_key_pem: Optional[str],
) -> None:
    from persistence.sqlalchemy.data import DataFile as DataFileEntity
    from persistence.sqlalchemy.data import TLSConfigSAE as TLSConfigSAEEntity

    cert_file = DataFileEntity(
        path=f"sae/{sae_entity.sae_id}/cert.pem",
        data=certificate_pem,
    )
    key_file = DataFileEntity(
        path=f"sae/{sae_entity.sae_id}/key.pem",
        data=private_key_pem or "",
    )
    ca_file = DataFileEntity(
        path=f"sae/{sae_entity.sae_id}/ca.pem",
        data=ca_chain_pem,
    )
    session.add_all([cert_file, key_file, ca_file])
    session.flush()

    if sae_entity.tls_id:
        tls_entity = session.get(TLSConfigSAEEntity, int(sae_entity.tls_id))
        if tls_entity is not None:
            tls_entity.cert_id = cert_file.id
            tls_entity.key_id = key_file.id
            tls_entity.ca_certs_id = ca_file.id
            tls_entity.use_client_cert = True
            session.flush()
            return

    tls_entity = TLSConfigSAEEntity(
        cert_id=cert_file.id,
        key_id=key_file.id,
        ca_certs_id=ca_file.id,
        use_client_cert=True,
    )
    session.add(tls_entity)
    session.flush()
    sae_entity.tls_id = tls_entity.id


def _persist_editor_topology(simulation_id: int, topology_json: str, name: str, description: Optional[str]) -> None:
    from persistence.sqlalchemy.data import Simulation as SimulationEntity

    with _sqlalchemy_uow_session() as (uow, session):
        entity = session.get(SimulationEntity, simulation_id)
        if entity is None:
            raise HTTPException(status_code=404, detail="Simulation not found")
        entity.name = name
        entity.description = description
        entity.editor_topology_json = topology_json
        entity.updated_at = datetime.now(timezone.utc)
        uow.commit()


def _edge_pair_key(source_uid: str, target_uid: str) -> tuple[str, str]:
    return (min(source_uid, target_uid), max(source_uid, target_uid))


def _links_by_pair(links: list[WebLinkInput]) -> dict[tuple[str, str], WebLinkInput]:
    mapping: dict[tuple[str, str], WebLinkInput] = {}
    for link in links:
        key = _edge_pair_key(link.source_uid, link.target_uid)
        if key not in mapping:
            mapping[key] = link
    return mapping


def _node_snapshot(nodes: list[WebNodeInput]) -> dict[str, tuple[int, str, float, float]]:
    return {
        node.uid: (int(node.node_id), str(node.label), float(node.x), float(node.y))
        for node in nodes
    }


def _is_qkd_like(link_type: str) -> bool:
    return link_type in {"QKD", "HYBRID"}


def _link_qkd_signature(link: WebLinkInput) -> tuple[int, int, float, float]:
    return (
        _link_qkd_distance(link),
        _link_qkd_buffer(link),
        round(_link_qkd_rate_r0(link), 6),
        round(_link_qkd_rate_alpha(link), 6),
    )


def _extract_node_id_from_label(label: str) -> Optional[int]:
    text = str(label or "").strip()
    if not text.upper().startswith("DKMS-"):
        return None
    try:
        value = int(text.split("-", 1)[1])
    except (TypeError, ValueError):
        return None
    return value if value > 0 else None


def _validate_running_patch_policy(
    current_dto: WebSimulationDTO,
    payload: WebSimulationUpsertRequest,
) -> list[dict[str, Any]]:
    if current_dto.sdn.model_dump() != payload.sdn.model_dump():
        raise HTTPException(
            status_code=409,
            detail="Cannot modify SDN configuration while simulation is running",
        )

    if _node_snapshot(current_dto.nodes) != _node_snapshot(payload.nodes):
        raise HTTPException(
            status_code=409,
            detail="Cannot modify nodes while simulation is running",
        )

    current_links = _links_by_pair(current_dto.links)
    desired_links = _links_by_pair(payload.links)
    operations: list[dict[str, Any]] = []

    for key in sorted(set(current_links) | set(desired_links)):
        current_link = current_links.get(key)
        desired_link = desired_links.get(key)

        if current_link is None and desired_link is not None:
            if desired_link.link_type == "PQC":
                operations.append({"kind": "create_pqc", "pair": key, "desired": desired_link})
                continue
            raise HTTPException(
                status_code=409,
                detail=f"Cannot create non-PQC link while running ({key[0]} <-> {key[1]})",
            )

        if current_link is not None and desired_link is None:
            if current_link.link_type == "PQC":
                operations.append({"kind": "delete_pqc", "pair": key, "current": current_link})
                continue
            raise HTTPException(
                status_code=409,
                detail=f"Cannot delete non-PQC link while running ({key[0]} <-> {key[1]})",
            )

        if current_link is None or desired_link is None:
            continue

        if current_link.link_type == desired_link.link_type:
            if _is_qkd_like(current_link.link_type) and _link_qkd_signature(current_link) != _link_qkd_signature(
                desired_link
            ):
                raise HTTPException(
                    status_code=409,
                    detail=f"Cannot modify QKD parameters while running ({key[0]} <-> {key[1]})",
                )
            continue

        if current_link.link_type == "QKD" and desired_link.link_type == "HYBRID":
            if _link_qkd_signature(current_link) != _link_qkd_signature(desired_link):
                raise HTTPException(
                    status_code=409,
                    detail=f"Cannot modify QKD parameters while converting to HYBRID ({key[0]} <-> {key[1]})",
                )
            operations.append({"kind": "qkd_to_hybrid", "pair": key, "current": current_link, "desired": desired_link})
            continue

        if current_link.link_type == "HYBRID" and desired_link.link_type == "QKD":
            if _link_qkd_signature(current_link) != _link_qkd_signature(desired_link):
                raise HTTPException(
                    status_code=409,
                    detail=f"Cannot modify QKD parameters while converting to QKD ({key[0]} <-> {key[1]})",
                )
            operations.append({"kind": "hybrid_to_qkd", "pair": key, "current": current_link, "desired": desired_link})
            continue

        if desired_link.link_type != "PQC":
            raise HTTPException(
                status_code=409,
                detail=f"Cannot create non-PQC link while running ({key[0]} <-> {key[1]})",
            )

        raise HTTPException(
            status_code=409,
            detail=(
                "Unsupported link transition while running "
                f"({current_link.link_type} -> {desired_link.link_type}) on {key[0]} <-> {key[1]}"
            ),
        )

    return operations


def _replace_running_simulation_graph(
    owner_id: int,
    simulation_id: int,
    payload: WebSimulationUpsertRequest,
    current: ModelSimulation,
) -> ModelSimulation:
    from persistence.sqlalchemy.data import (
        ChannelTypeEnum,
        Host as HostEntity,
        KME as KMEEntity,
        QKC as QKCEntity,
        Simulation as SimulationEntity,
    )

    _validate_web_topology(payload)
    current_dto = _simulation_to_web_dto(current)
    operations = _validate_running_patch_policy(current_dto, payload)
    topology_json = _topology_json_from_request(payload)
    if not operations and payload.name.strip() == current.name and payload.description == current.description:
        return current

    node_id_by_uid = {node.uid: int(node.node_id) for node in payload.nodes}

    qkc_to_dkms: dict[int, int] = {}
    for dkms in current.list_dkms:
        qkc_id = getattr(getattr(getattr(dkms, "orr", None), "qkc", None), "id", None)
        dkms_id = getattr(dkms, "id", None)
        if qkc_id is None or dkms_id is None:
            continue
        qkc_to_dkms[int(qkc_id)] = int(dkms_id)

    affected_dkms_ids: set[int] = set()

    with _sqlalchemy_uow_session() as (uow, session):
        simulation_entity = session.get(SimulationEntity, simulation_id)
        if simulation_entity is None:
            raise HTTPException(status_code=404, detail="Simulation not found")
        if int(simulation_entity.id_user) != int(owner_id):
            raise HTTPException(status_code=403, detail="Forbidden")

        kme_rows = (
            session.query(KMEEntity)
            .join(QKCEntity, KMEEntity.local_qkc_id == QKCEntity.id)
            .join(HostEntity, QKCEntity.id_host == HostEntity.id)
            .filter(HostEntity.id_simulation == int(simulation_id))
            .all()
        )
        if not kme_rows and operations:
            raise HTTPException(status_code=409, detail="No KME links found for running simulation")

        label_by_qkc: dict[int, str] = {}
        for row in kme_rows:
            neighbor_label = str(getattr(row, "neighbor_QKD", "") or "").strip()
            if not neighbor_label:
                continue
            label_by_qkc[int(row.neighbor_qkc_id)] = neighbor_label
        qkc_by_label = {label: qkc_id for qkc_id, label in label_by_qkc.items()}

        templates_by_local_qkc: dict[int, Any] = {}
        rows_by_direction: dict[tuple[int, int], list[Any]] = {}
        for row in kme_rows:
            local_id = int(row.local_qkc_id)
            neighbor_id = int(row.neighbor_qkc_id)
            templates_by_local_qkc.setdefault(local_id, row)
            rows_by_direction.setdefault((local_id, neighbor_id), []).append(row)

        def uid_to_qkc(uid: str) -> int:
            node_id = node_id_by_uid.get(uid)
            if node_id is None:
                raise HTTPException(status_code=409, detail=f"Unknown node uid in running patch: {uid}")
            label = _node_label(node_id)
            qkc_id = qkc_by_label.get(label)
            if qkc_id is None:
                raise HTTPException(
                    status_code=409,
                    detail=f"Cannot resolve runtime QKC for node_id={node_id} (label={label})",
                )
            return int(qkc_id)

        def set_link_kind(row: Any, kind: str) -> None:
            if kind == "PQC":
                row.channel_type = ChannelTypeEnum.PQC_SIMULATION
                row.channel_distance = 0
                row.channel_quditto_max_buffer_size = 10000
                row.channel_quditto_rate_r0 = DEFAULT_QUDITTO_RATE_R0
                row.channel_quditto_rate_alpha = DEFAULT_QUDITTO_RATE_ALPHA
                row.pqc_simulation = True
                row.hybrid_enabled = False
                return
            row.channel_type = ChannelTypeEnum.QKD
            row.pqc_simulation = False
            row.hybrid_enabled = kind == "HYBRID"

        for op in operations:
            left_uid, right_uid = op["pair"]
            left_qkc = uid_to_qkc(left_uid)
            right_qkc = uid_to_qkc(right_uid)
            forward = rows_by_direction.get((left_qkc, right_qkc), [])
            backward = rows_by_direction.get((right_qkc, left_qkc), [])
            all_rows = [*forward, *backward]

            if op["kind"] == "create_pqc":
                if all_rows:
                    raise HTTPException(
                        status_code=409,
                        detail=f"Link already exists while trying to create PQC ({left_uid} <-> {right_uid})",
                    )
                left_template = templates_by_local_qkc.get(left_qkc)
                right_template = templates_by_local_qkc.get(right_qkc)
                if left_template is None or right_template is None:
                    raise HTTPException(
                        status_code=409,
                        detail=f"Cannot create PQC link without KME template ({left_uid} <-> {right_uid})",
                    )

                left_label = _node_label(node_id_by_uid[left_uid])
                right_label = _node_label(node_id_by_uid[right_uid])
                left_row = KMEEntity(
                    local_qkc_id=left_qkc,
                    neighbor_qkc_id=right_qkc,
                    cert_id=left_template.cert_id,
                    key_id=left_template.key_id,
                    url_node_QKD=left_template.url_node_QKD,
                    neighbor_QKD=right_label,
                    etsi=left_template.etsi,
                    channel_type=ChannelTypeEnum.PQC_SIMULATION,
                    channel_distance=0,
                    channel_quditto_max_buffer_size=10000,
                    channel_quditto_rate_r0=DEFAULT_QUDITTO_RATE_R0,
                    channel_quditto_rate_alpha=DEFAULT_QUDITTO_RATE_ALPHA,
                    pqc_simulation=True,
                    hybrid_enabled=False,
                    pqc_kme_port=int(getattr(left_template, "pqc_kme_port", 6000) or 6000),
                )
                right_row = KMEEntity(
                    local_qkc_id=right_qkc,
                    neighbor_qkc_id=left_qkc,
                    cert_id=right_template.cert_id,
                    key_id=right_template.key_id,
                    url_node_QKD=right_template.url_node_QKD,
                    neighbor_QKD=left_label,
                    etsi=right_template.etsi,
                    channel_type=ChannelTypeEnum.PQC_SIMULATION,
                    channel_distance=0,
                    channel_quditto_max_buffer_size=10000,
                    channel_quditto_rate_r0=DEFAULT_QUDITTO_RATE_R0,
                    channel_quditto_rate_alpha=DEFAULT_QUDITTO_RATE_ALPHA,
                    pqc_simulation=True,
                    hybrid_enabled=False,
                    pqc_kme_port=int(getattr(right_template, "pqc_kme_port", 6000) or 6000),
                )
                session.add(left_row)
                session.add(right_row)
                session.flush()
                rows_by_direction.setdefault((left_qkc, right_qkc), []).append(left_row)
                rows_by_direction.setdefault((right_qkc, left_qkc), []).append(right_row)
                templates_by_local_qkc.setdefault(left_qkc, left_row)
                templates_by_local_qkc.setdefault(right_qkc, right_row)
            elif op["kind"] == "delete_pqc":
                if not all_rows:
                    continue
                for row in all_rows:
                    raw_channel = getattr(getattr(row, "channel_type", None), "value", getattr(row, "channel_type", None))
                    is_pqc = bool(getattr(row, "pqc_simulation", False)) or str(raw_channel) == ChannelTypeEnum.PQC_SIMULATION.value
                    if not is_pqc or bool(getattr(row, "hybrid_enabled", False)):
                        raise HTTPException(
                            status_code=409,
                            detail=f"Cannot delete non-PQC link while running ({left_uid} <-> {right_uid})",
                        )
                for row in all_rows:
                    session.delete(row)
                rows_by_direction.pop((left_qkc, right_qkc), None)
                rows_by_direction.pop((right_qkc, left_qkc), None)
            elif op["kind"] == "qkd_to_hybrid":
                if not all_rows:
                    raise HTTPException(
                        status_code=409,
                        detail=f"Missing runtime link to convert QKD->HYBRID ({left_uid} <-> {right_uid})",
                    )
                for row in all_rows:
                    set_link_kind(row, "HYBRID")
            elif op["kind"] == "hybrid_to_qkd":
                if not all_rows:
                    raise HTTPException(
                        status_code=409,
                        detail=f"Missing runtime link to convert HYBRID->QKD ({left_uid} <-> {right_uid})",
                    )
                for row in all_rows:
                    set_link_kind(row, "QKD")

            left_dkms = qkc_to_dkms.get(left_qkc)
            right_dkms = qkc_to_dkms.get(right_qkc)
            if left_dkms is not None:
                affected_dkms_ids.add(int(left_dkms))
            if right_dkms is not None:
                affected_dkms_ids.add(int(right_dkms))

        simulation_entity.name = payload.name.strip()
        simulation_entity.description = payload.description
        simulation_entity.editor_topology_json = topology_json
        simulation_entity.updated_at = datetime.now(timezone.utc)
        uow.commit()

    if affected_dkms_ids:
        runtime_orchestator = _build_orchestator()
        runtime_errors: list[str] = []
        for dkms_id in sorted(affected_dkms_ids):
            try:
                runtime_orchestator.stop_dkms(owner_id, simulation_id, dkms_id)
                runtime_orchestator.start_dkms(owner_id, simulation_id, dkms_id)
            except Exception as exc:  # noqa: BLE001
                runtime_errors.append(f"DKMS {dkms_id}: {exc}")
        if runtime_errors:
            raise HTTPException(
                status_code=500,
                detail="Runtime link changes were persisted but failed to apply: " + " | ".join(runtime_errors),
            )

    refreshed_orchestator = _build_orchestator()
    try:
        return refreshed_orchestator.get_simulation_id(owner_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc


def _replace_simulation_graph(
    owner_id: int,
    simulation_id: int,
    payload: WebSimulationUpsertRequest,
) -> ModelSimulation:
    orchestator = _build_orchestator()

    try:
        current = orchestator.get_simulation_id(owner_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    if current.status == SimulationStatus.RUNNING:
        return _replace_running_simulation_graph(owner_id, simulation_id, payload, current)

    desired = _build_model_simulation_from_web(owner_id, payload)
    topology_json = _topology_json_from_request(payload)

    with orchestator.uow:
        session = getattr(orchestator.uow, "_session", None)
        if session is None:
            raise HTTPException(
                status_code=500,
                detail="PATCH /orch/web/simulations requires sqlalchemy backend",
            )

        from persistence.sqlalchemy.data import AgentController as AgentControllerEntity
        from persistence.sqlalchemy.data import DKMS as DKMSEntity
        from persistence.sqlalchemy.data import Host as HostEntity
        from persistence.sqlalchemy.data import SAE as SAEEntity
        from persistence.sqlalchemy.data import Simulation as SimulationEntity
        from persistence.sqlalchemy.mappers import Entity2Model, Model2Entity

        simulation_entity = session.get(SimulationEntity, simulation_id)
        if simulation_entity is None:
            raise HTTPException(status_code=404, detail="Simulation not found")
        if int(simulation_entity.id_user) != int(owner_id):
            raise HTTPException(status_code=403, detail="Forbidden")

        existing_sae_rows = (
            session.query(SAEEntity.id, SAEEntity.dkms_id)
            .filter(SAEEntity.simulation_id == int(simulation_id))
            .all()
        )
        old_dkms_rows = (
            session.query(DKMSEntity, HostEntity)
            .join(HostEntity, DKMSEntity.id_host == HostEntity.id)
            .filter(HostEntity.id_simulation == int(simulation_id))
            .all()
        )
        old_endpoint_by_dkms_id: dict[int, tuple[str, int]] = {}
        for dkms_row, host_row in old_dkms_rows:
            if dkms_row is None or host_row is None:
                continue
            old_endpoint_by_dkms_id[int(dkms_row.id)] = (
                str(host_row.ip),
                int(host_row.port),
            )
        old_binding_by_sae_row_id: dict[int, tuple[str, int]] = {}
        for sae_row_id, dkms_fk in existing_sae_rows:
            if sae_row_id is None or dkms_fk is None:
                continue
            endpoint = old_endpoint_by_dkms_id.get(int(dkms_fk))
            if endpoint is None:
                continue
            old_binding_by_sae_row_id[int(sae_row_id)] = endpoint

        # Evita borrado cascada de SAE al eliminar hosts/agent_controller.
        session.query(SAEEntity).filter(
            SAEEntity.simulation_id == int(simulation_id)
        ).update(
            {
                SAEEntity.agent_dkms_id: None,
                SAEEntity.dkms_id: None,
                # El SDN se recrea durante replace de topología.
                # Desacoplar previamente evita conflictos FK y borrados indirectos.
                SAEEntity.sdn_id: None,
            },
            synchronize_session=False,
        )
        session.flush()

        simulation_entity.name = payload.name.strip()
        simulation_entity.description = payload.description
        simulation_entity.editor_topology_json = topology_json
        simulation_entity.updated_at = datetime.now(timezone.utc)

        for host in list(simulation_entity.hosts or []):
            session.delete(host)
        session.flush()

        qkc_id_map: Dict[int, int] = {}
        qkc_cache: Dict[str, ModelQKC] = {}
        orr_cache: Dict[str, ModelORR] = {}
        queued_kmes: list[tuple[str, KMEConfig]] = []
        persisted_dkms: list[ModelDKMS] = []

        for dkms_model in desired.list_dkms:
            dkms_model = orchestator._bind_host_to_simulation(dkms_model, simulation_id)

            if dkms_model.orr is not None:
                orr_model = orchestator._bind_host_to_simulation(dkms_model.orr, simulation_id)

                if orr_model.qkc is not None:
                    qkc_model = orchestator._bind_host_to_simulation(orr_model.qkc, simulation_id)
                    qkc_key = orchestator._entity_cache_key(qkc_model)
                    saved_qkc = qkc_cache.get(qkc_key)
                    if saved_qkc is None:
                        qkc_kmes = list(qkc_model.kmes)
                        # Force insert of a new QKC row when replacing topology.
                        # Reusing deterministic ids here can trigger SQLAlchemy to update
                        # existing KME relations and set local_qkc_id to NULL.
                        original_qkc_id = qkc_model.id
                        qkc_to_save = qkc_model.model_copy(
                            update={
                                "id": None,
                                "id_host": None,
                                "kmes": [],
                            }
                        )
                        saved_qkc = orchestator.uow.repos.qkcs.save(qkc_to_save)
                        qkc_cache[qkc_key] = saved_qkc
                        if original_qkc_id is not None and saved_qkc.id is not None:
                            qkc_id_map[original_qkc_id] = saved_qkc.id
                        for kme_model in qkc_kmes:
                            queued_kmes.append((qkc_key, kme_model))

                    if saved_qkc.id is None:
                        raise RuntimeError("No se pudo persistir el QKC de la simulacion")

                    orr_model = orr_model.model_copy(
                        update={"qkc_id": saved_qkc.id, "qkc": saved_qkc}
                    )

                saved_orr = orr_cache.get(orchestator._entity_cache_key(orr_model))
                if saved_orr is None:
                    saved_orr = orchestator.uow.repos.orrs.save(orr_model)
                    orr_cache[orchestator._entity_cache_key(orr_model)] = saved_orr

                if saved_orr.id is None:
                    raise RuntimeError("No se pudo persistir el ORR de la simulacion")

                dkms_model = dkms_model.model_copy(update={"orr_id": saved_orr.id, "orr": saved_orr})

            persisted_dkms.append(orchestator.uow.repos.dkms.save(dkms_model))

        persisted_sdn = orchestator._bind_host_to_simulation(desired.sdn, simulation_id)
        sdn_entity = session.merge(Model2Entity.sdn(persisted_sdn))
        session.flush()
        persisted_sdn = Entity2Model.sdn(sdn_entity)

        for qkc_key, kme_model in queued_kmes:
            owner_qkc = qkc_cache[qkc_key]
            if owner_qkc.id is None:
                raise RuntimeError("No se pudo resolver el QKC para persistir KME")

            local_qkc_id = qkc_id_map.get(kme_model.local_qkc_id, kme_model.local_qkc_id)
            neighbor_qkc_id = qkc_id_map.get(kme_model.neighbor_qkc_id, kme_model.neighbor_qkc_id)

            if local_qkc_id is None:
                local_qkc_id = owner_qkc.id
            if neighbor_qkc_id is None:
                raise ValueError("neighbor_qkc_id es obligatorio en la configuracion KME")

            session.merge(
                Model2Entity.kme(
                    kme_model.model_copy(
                        update={
                            "local_qkc_id": local_qkc_id,
                            "neighbor_qkc_id": neighbor_qkc_id,
                        }
                    )
                )
            )
        session.flush()

        new_dkms_rows = (
            session.query(DKMSEntity, HostEntity)
            .join(HostEntity, DKMSEntity.id_host == HostEntity.id)
            .filter(HostEntity.id_simulation == int(simulation_id))
            .all()
        )
        new_dkms_by_endpoint: dict[tuple[str, int], int] = {}
        for dkms_row, host_row in new_dkms_rows:
            if dkms_row is None or host_row is None:
                continue
            new_dkms_by_endpoint[(str(host_row.ip), int(host_row.port))] = int(dkms_row.id)

        controllers_by_dkms_id: dict[int, int] = {}
        for new_dkms_id in sorted(set(new_dkms_by_endpoint.values())):
            controller = (
                session.query(AgentControllerEntity)
                .filter(AgentControllerEntity.id_dkms == int(new_dkms_id))
                .order_by(AgentControllerEntity.id.asc())
                .first()
            )
            if controller is None:
                controller = AgentControllerEntity(
                    id_dkms=int(new_dkms_id),
                    id_sdn=(int(persisted_sdn.id) if getattr(persisted_sdn, "id", None) is not None else None),
                    id_host=None,
                )
                session.add(controller)
                session.flush()
            elif getattr(persisted_sdn, "id", None) is not None:
                controller.id_sdn = int(persisted_sdn.id)
                session.flush()

            if getattr(controller, "id", None) is not None:
                controllers_by_dkms_id[int(new_dkms_id)] = int(controller.id)

        now_utc = datetime.now(timezone.utc)
        for sae_row_id, old_endpoint in old_binding_by_sae_row_id.items():
            if old_endpoint is None:
                continue
            new_dkms_id = new_dkms_by_endpoint.get(old_endpoint)
            if new_dkms_id is None:
                continue
            update_payload: dict[Any, Any] = {
                SAEEntity.dkms_id: int(new_dkms_id),
                SAEEntity.agent_dkms_id: controllers_by_dkms_id.get(int(new_dkms_id)),
                SAEEntity.updated_at: now_utc,
            }
            if getattr(persisted_sdn, "id", None) is not None:
                update_payload[SAEEntity.sdn_id] = int(persisted_sdn.id)
            session.query(SAEEntity).filter(SAEEntity.id == int(sae_row_id)).update(
                update_payload,
                synchronize_session=False,
            )
        session.flush()

        current_model = orchestator.uow.repos.simulations.get(simulation_id)
        if current_model is None:
            raise RuntimeError("Simulation not found after graph replacement")

        updated_model = current_model.model_copy(
            update={
                "name": payload.name.strip(),
                "description": payload.description,
                "editor_topology_json": topology_json,
                "list_dkms": persisted_dkms,
                "sdn": persisted_sdn,
            }
        )
        orchestator.uow.repos.simulations.save(updated_model)
        orchestator.uow.commit()

        persisted = orchestator.uow.repos.simulations.get(simulation_id)
        if persisted is None:
            raise RuntimeError("Simulation not found after update")
        return persisted


def _create_run_record(
    simulation_id: int,
    status_text: Literal["QUEUED", "RUNNING", "DONE", "FAILED"],
    message: Optional[str] = None,
    queued_at: Optional[datetime] = None,
    started_at: Optional[datetime] = None,
    finished_at: Optional[datetime] = None,
) -> WebSimulationRunDTO:
    from persistence.sqlalchemy.data import SimulationRun as SimulationRunEntity
    from persistence.sqlalchemy.data import SimulationRunStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        entity = SimulationRunEntity(
            simulation_id=int(simulation_id),
            status=SimulationRunStatusEnum[status_text],
            message=message,
            queued_at=queued_at,
            started_at=started_at,
            finished_at=finished_at,
        )
        session.add(entity)
        session.flush()
        session.refresh(entity)
        uow.commit()
        return WebSimulationRunDTO(
            id=int(entity.id),
            simulation_id=int(entity.simulation_id),
            status=str(getattr(entity.status, "value", entity.status)),
            message=entity.message,
            queued_at=_to_iso(entity.queued_at) if entity.queued_at else None,
            started_at=_to_iso(entity.started_at) if entity.started_at else None,
            finished_at=_to_iso(entity.finished_at) if entity.finished_at else None,
            created_at=_to_iso(entity.created_at),
        )


def _update_run_record(
    run_id: int,
    status_text: Literal["QUEUED", "RUNNING", "DONE", "FAILED"],
    message: Optional[str] = None,
    started_at: Optional[datetime] = None,
    finished_at: Optional[datetime] = None,
) -> None:
    from persistence.sqlalchemy.data import SimulationRun as SimulationRunEntity
    from persistence.sqlalchemy.data import SimulationRunStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        entity = session.get(SimulationRunEntity, int(run_id))
        if entity is None:
            return
        entity.status = SimulationRunStatusEnum[status_text]
        if message is not None:
            entity.message = message
        if started_at is not None:
            entity.started_at = started_at
        if finished_at is not None:
            entity.finished_at = finished_at
        uow.commit()


def _list_run_records(simulation_id: int) -> list[WebSimulationRunDTO]:
    from persistence.sqlalchemy.data import SimulationRun as SimulationRunEntity

    with _sqlalchemy_uow_session() as (_, session):
        entities = (
            session.query(SimulationRunEntity)
            .filter(SimulationRunEntity.simulation_id == int(simulation_id))
            .order_by(SimulationRunEntity.created_at.desc(), SimulationRunEntity.id.desc())
            .all()
        )
        return [
            WebSimulationRunDTO(
                id=int(entity.id),
                simulation_id=int(entity.simulation_id),
                status=str(getattr(entity.status, "value", entity.status)),
                message=entity.message,
                queued_at=_to_iso(entity.queued_at) if entity.queued_at else None,
                started_at=_to_iso(entity.started_at) if entity.started_at else None,
                finished_at=_to_iso(entity.finished_at) if entity.finished_at else None,
                created_at=_to_iso(entity.created_at),
            )
            for entity in entities
        ]


app = FastAPI(title="Orchestator API", version="1.0.0")


@app.exception_handler(OperationalError)
def handle_db_operational_error(_: Any, __: OperationalError) -> JSONResponse:
    return JSONResponse(
        status_code=503,
        content={"detail": "Database temporarily unavailable. Retry in a few seconds."},
    )


@app.get("/orch/health")
def health() -> dict[str, str]:
    return {"status": "ok"}


@app.post(
    "/orch/simulations",
    response_model=ModelSimulation,
    status_code=status.HTTP_201_CREATED,
)
def create_simulation(
    data: dict = Body(...),
    x_user_id: int = Depends(_require_user_id),
) -> ModelSimulation:
    payload = dict(data)
    payload["id_user"] = x_user_id
    payload.pop("user", None)

    try:
        simulation = ModelSimulation.model_validate(payload)
    except ValidationError as exc:
        raise HTTPException(
            status_code=status.HTTP_422_UNPROCESSABLE_ENTITY,
            detail=exc.errors(),
        ) from exc

    orchestator = _build_orchestator()
    try:
        return orchestator.create_simulation(simulation)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc


@app.get(
    "/orch/simulations",
    response_model=list[ModelSimulation],
)
def get_simulations(
    x_user_id: int = Depends(_require_user_id),
) -> list[ModelSimulation]:
    orchestator = _build_orchestator()
    try:
        return orchestator.get_simulations(x_user_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc


@app.get(
    "/orch/api/sim/{simulation_id}",
    response_model=ModelSimulation,
)
def get_simulation_id(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> ModelSimulation:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        return orchestator.get_simulation_id(x_user_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc


@app.post(
    "/orch/api/sim/{simulation_id}/run",
    response_model=SimulationActionResponse,
)
def run_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        orchestator.run_simulation(str(x_user_id), simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action="run",
    )


@app.post(
    "/orch/api/sim/{simulation_id}/stop",
    response_model=SimulationActionResponse,
)
def stop_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        orchestator.stop_simulation(str(x_user_id), simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action="stop",
    )


@app.post(
    "/orch/api/sim/{simulation_id}/dkms/{dkms_id}/stop",
    response_model=SimulationActionResponse,
)
def stop_dkms_in_simulation(
    simulation_id: int,
    dkms_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        orchestator.stop_dkms(str(x_user_id), simulation_id, dkms_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action=f"stop-dkms-{dkms_id}",
    )


@app.post(
    "/orch/api/sim/{simulation_id}/dkms/{dkms_id}/start",
    response_model=SimulationActionResponse,
)
def start_dkms_in_simulation(
    simulation_id: int,
    dkms_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        orchestator.start_dkms(str(x_user_id), simulation_id, dkms_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action=f"start-dkms-{dkms_id}",
    )


@app.delete(
    "/orch/api/sim/{simulation_id}",
    response_model=SimulationActionResponse,
)
def delete_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        orchestator.delete_simulation(str(x_user_id), simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action="delete",
    )


def _loadtest_ingress_scheme() -> str:
    raw = (os.environ.get("K8S_INGRESS_SCHEME", "") or "").strip().lower()
    if raw in {"http", "https"}:
        return raw
    return "https"


def _loadtest_management_base_url() -> str:
    host = (K8S_INGRESS_HOST or "").strip()
    if not host or host == "*":
        host = "dkms2.pablopiorejoiglesias.es"
    if host.startswith(("http://", "https://")):
        return host.rstrip("/")
    return f"{_loadtest_ingress_scheme()}://{host}"


def _loadtest_runtime_base_url() -> str:
    host = (K8S_RUNTIME_INGRESS_HOST or K8S_INGRESS_HOST or "").strip()
    if not host or host == "*":
        host = "api.pablopiorejoiglesias.es"
    if host.startswith(("http://", "https://")):
        return host.rstrip("/")
    return f"{_loadtest_ingress_scheme()}://{host}"


def _loadtest_grafana_url(simulation_id: int, test_id: str) -> str:
    base = _loadtest_management_base_url()
    return (
        f"{base}/api/sim/{simulation_id}/grafana/d/dkms-loadtest"
        f"?var-test_id={test_id}&refresh=5s&from=now-30m&to=now"
    )


def _loadtest_test_id(simulation_id: int) -> str:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
    rand = secrets.token_hex(3)
    return f"ramp-sim{simulation_id}-{stamp}-{rand}"


def _loadtest_authz_internal_url() -> str:
    raw = os.environ.get(
        "LOADTEST_AUTHZ_INTERNAL_URL",
        "http://authz.dkms-main-ns.svc.cluster.local:8081",
    ).strip()
    return raw.rstrip("/")


def _loadtest_service_login() -> str:
    """Mint a short-lived AUTHZ token using the orchestator's service credentials.

    Credentials come from env vars LOADTEST_SERVICE_USERNAME /
    LOADTEST_SERVICE_PASSWORD, meant to be sourced from a Secret at deploy
    time. The web user never handles them.
    """
    username = os.environ.get("LOADTEST_SERVICE_USERNAME", "config_user").strip()
    password = os.environ.get("LOADTEST_SERVICE_PASSWORD", "config_password")
    if not username:
        raise HTTPException(status_code=500, detail="loadtest service username not configured")
    url = f"{_loadtest_authz_internal_url()}/login"
    data = json.dumps({"username": username, "password": password}).encode("utf-8")
    req = urllib_request.Request(
        url,
        data=data,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    try:
        with urllib_request.urlopen(req, timeout=15) as resp:
            body = resp.read().decode("utf-8")
    except urllib_error.HTTPError as exc:
        detail = exc.read().decode("utf-8", errors="replace")[:200]
        raise HTTPException(
            status_code=502,
            detail=f"authz login failed ({exc.code}): {detail}",
        ) from exc
    except urllib_error.URLError as exc:
        raise HTTPException(status_code=502, detail=f"authz unreachable: {exc}") from exc
    try:
        payload = json.loads(body)
    except ValueError as exc:
        raise HTTPException(status_code=502, detail="authz returned non-JSON") from exc
    token = str(payload.get("access_token") or "").strip()
    if not token:
        raise HTTPException(status_code=502, detail="authz did not return access_token")
    return token


def _build_loadtest_env(
    *,
    simulation_id: int,
    test_id: str,
    payload: "LoadTestCreateRequest",
    authz_token: str,
) -> Dict[str, str]:
    offset = payload.offset_seconds if payload.offset_seconds is not None else payload.interval_seconds
    orch_internal = os.environ.get(
        "LOADTEST_ORCH_INTERNAL_URL",
        "http://orchestator.dkms-main-ns.svc.cluster.local:8080",
    )
    return {
        "TEST_ORCH_INTERNAL_URL": orch_internal,
        "TEST_ID": test_id,
        "SIM_ID": str(int(simulation_id)),
        "START_SAES": str(int(payload.start_saes)),
        "END_SAES": str(int(payload.end_saes)),
        "STEP_SAES": str(int(payload.step_saes)),
        "INTERVAL_SECONDS": f"{float(payload.interval_seconds):g}",
        "OFFSET_SECONDS": f"{float(offset):g}",
        "WARMUP_SECONDS": f"{float(payload.warmup_seconds):g}",
        "KEY_SIZE_BITS": str(int(payload.key_size_bits)),
        "PER_SAE_LAMBDA": f"{float(payload.per_sae_lambda):g}",
        "REQUEST_TIMEOUT_SECONDS": str(int(payload.request_timeout_seconds)),
        "INGRESS_CONTROLLER_URL": _loadtest_management_base_url(),
        "RUNTIME_BASE_URL": _loadtest_runtime_base_url(),
        "AUTHZ_TOKEN": authz_token,
        "METRICS_PORT": "9095",
        "TEST_REQUESTS_VERIFY": "false",
        # Bypass the SDN /dkms/ endpoint, which stalls under the 50-KME
        # bootstrap storm. The orchestator already has the full list in its
        # simulation entity, so /orch/simulations answers in ~1s.
        "TEST_PREFER_ORCH_DKMS_LIST": "true",
        # The runner emits requests.csv + sae_timeline.csv in this dir.
        # The pod spec mounts an emptyDir at exactly this path so the
        # files persist until the Deployment is deleted and can be
        # extracted with `kubectl cp` (see Makefile: loadtest-download).
        "OUTPUT_DIR": "/var/loadtest-output",
    }


def _loadtest_info_from_summary(
    *,
    simulation_id: int,
    test_id: str,
    summary: Dict[str, Any],
) -> LoadTestInfo:
    return LoadTestInfo(
        test_id=test_id,
        deployment_name=str(summary.get("name") or PodLoadtest.deployment_name_for(test_id)),
        simulation_id=int(simulation_id),
        grafana_url=_loadtest_grafana_url(simulation_id, test_id),
        replicas=int(summary.get("replicas") or 0),
        ready_replicas=int(summary.get("ready_replicas") or 0),
        available_replicas=int(summary.get("available_replicas") or 0),
        created_at=summary.get("created_at"),
    )


@app.post(
    "/orch/api/sim/{simulation_id}/tests",
    response_model=LoadTestInfo,
    status_code=status.HTTP_201_CREATED,
)
@app.post(
    "/orch/web/simulations/{simulation_id}/tests",
    response_model=LoadTestInfo,
    status_code=status.HTTP_201_CREATED,
    include_in_schema=False,
)
def start_loadtest(
    simulation_id: int,
    payload: LoadTestCreateRequest,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> LoadTestInfo:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (_, session):
        _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )

    test_id = _loadtest_test_id(int(simulation_id))
    authz_token = _loadtest_service_login()
    env_vars = _build_loadtest_env(
        simulation_id=int(simulation_id),
        test_id=test_id,
        payload=payload,
        authz_token=authz_token,
    )
    pull_secret = DOCKER_HUB_SECRET_NAME if DOCKER_HUB_SECRET_NAME else None
    try:
        pod = PodLoadtest(str(simulation_id))
        deployment_name = pod.deploy_test(
            test_id=test_id,
            env_vars=env_vars,
            image_pull_secret=pull_secret,
        )
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"loadtest deploy failed: {exc}") from exc

    return LoadTestInfo(
        test_id=test_id,
        deployment_name=deployment_name,
        simulation_id=int(simulation_id),
        grafana_url=_loadtest_grafana_url(int(simulation_id), test_id),
        replicas=1,
        ready_replicas=0,
        available_replicas=0,
    )


@app.get(
    "/orch/api/sim/{simulation_id}/tests",
    response_model=list[LoadTestInfo],
)
@app.get(
    "/orch/web/simulations/{simulation_id}/tests",
    response_model=list[LoadTestInfo],
    include_in_schema=False,
)
def list_loadtests(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> list[LoadTestInfo]:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (_, session):
        _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
    try:
        pod = PodLoadtest(str(simulation_id))
        summaries = pod.list_tests()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"loadtest list failed: {exc}") from exc
    return [
        _loadtest_info_from_summary(
            simulation_id=int(simulation_id),
            test_id=str(s.get("test_id") or ""),
            summary=s,
        )
        for s in summaries
    ]


@app.delete(
    "/orch/api/sim/{simulation_id}/tests/{test_id}",
    response_model=SimulationActionResponse,
)
@app.delete(
    "/orch/web/simulations/{simulation_id}/tests/{test_id}",
    response_model=SimulationActionResponse,
    include_in_schema=False,
)
def stop_loadtest(
    simulation_id: int,
    test_id: str,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (_, session):
        _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
    try:
        pod = PodLoadtest(str(simulation_id))
        pod.delete_test(test_id)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(status_code=500, detail=f"loadtest stop failed: {exc}") from exc
    return SimulationActionResponse(
        status="ok",
        simulation_id=int(simulation_id),
        action=f"stop-test-{test_id}",
    )


@app.post(
    "/orch/api/sim/{simulation_id}/sdn/sae",
    response_model=SDNSAEDTO,
    status_code=status.HTTP_201_CREATED,
)
@app.post(
    "/orch/api/sim/{simulation_id}/sdn/sae/",
    response_model=SDNSAEDTO,
    status_code=status.HTTP_201_CREATED,
    include_in_schema=False,
)
def create_sae_via_sdn_compat(
    simulation_id: int,
    payload: SDNSAECreatePayload,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SDNSAEDTO:
    from persistence.sqlalchemy.data import SAE as SAEEntity
    from persistence.sqlalchemy.data import SaeStatusEnum

    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (uow, session):
        simulation_entity = _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
        resolved_dkms = _resolve_dkms_from_sdn_payload(
            session,
            simulation_id=int(simulation_id),
            dkms_id=payload.dkms_id,
            dkms_target=payload.dkms_target,
        )
        resolved_dkms_id = int(resolved_dkms.id)

        existing = (
            session.query(SAEEntity)
            .filter(SAEEntity.simulation_id == int(simulation_id))
            .filter(SAEEntity.sae_id == payload.id)
            .one_or_none()
        )
        if existing is not None:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail=f"SAE '{payload.id}' is already registered in simulation {int(simulation_id)}",
            )

        controller = _resolve_agent_controller_for_dkms(session, dkms_id=resolved_dkms_id)
        entity = SAEEntity(
            sae_id=payload.id,
            display_name=payload.id,
            owner_user_id=int(x_user_id),
            simulation_id=int(simulation_id),
            dkms_id=resolved_dkms_id,
            status=SaeStatusEnum.PENDING_CERT,
            sdn_id=int(controller.id_sdn) if controller and controller.id_sdn is not None else None,
            agent_dkms_id=int(controller.id) if controller else None,
            tls_id=None,
        )
        session.add(entity)
        session.flush()

        if _simulation_is_running(simulation_entity):
            _sync_sae_binding_to_sdn(
                simulation_entity=simulation_entity,
                sae_id=payload.id,
                dkms_id=resolved_dkms_id,
                session=session,
            )

        session.flush()
        session.refresh(entity)
        uow.commit()
        return _build_sdn_sae_payload(session, sae_entity=entity)


@app.put(
    "/orch/api/sim/{simulation_id}/sdn/sae/{sae_id}",
    response_model=SDNSAEDTO,
)
def update_sae_via_sdn_compat(
    simulation_id: int,
    sae_id: str,
    payload: SDNSAEUpdatePayload,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SDNSAEDTO:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (uow, session):
        simulation_entity = _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
        entity = _require_owned_sae_in_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
            sae_id=sae_id,
        )
        resolved_dkms = _resolve_dkms_from_sdn_payload(
            session,
            simulation_id=int(simulation_id),
            dkms_id=payload.dkms_id,
            dkms_target=payload.dkms_target,
        )
        resolved_dkms_id = int(resolved_dkms.id)
        controller = _resolve_agent_controller_for_dkms(session, dkms_id=resolved_dkms_id)

        entity.dkms_id = resolved_dkms_id
        entity.agent_dkms_id = int(controller.id) if controller else None
        entity.sdn_id = int(controller.id_sdn) if controller and controller.id_sdn is not None else entity.sdn_id
        entity.updated_at = datetime.now(timezone.utc)
        session.flush()

        if _simulation_is_running(simulation_entity):
            _sync_sae_binding_to_sdn(
                simulation_entity=simulation_entity,
                sae_id=str(entity.sae_id or sae_id),
                dkms_id=resolved_dkms_id,
                session=session,
            )

        session.flush()
        session.refresh(entity)
        uow.commit()
        return _build_sdn_sae_payload(session, sae_entity=entity)


@app.delete(
    "/orch/api/sim/{simulation_id}/sdn/sae/{sae_id}",
    status_code=status.HTTP_204_NO_CONTENT,
)
def delete_sae_via_sdn_compat(
    simulation_id: int,
    sae_id: str,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> Response:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (uow, session):
        simulation_entity = _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
        entity = _require_owned_sae_in_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
            sae_id=sae_id,
        )
        resolved_sae_id = str(getattr(entity, "sae_id", "") or sae_id)

        if _simulation_is_running(simulation_entity):
            _delete_sae_binding_from_sdn(
                simulation_entity=simulation_entity,
                sae_id=resolved_sae_id,
            )

        session.delete(entity)
        session.flush()
        uow.commit()
        return Response(status_code=status.HTTP_204_NO_CONTENT)


@app.get(
    "/orch/api/sim/{simulation_id}/sdn/sae/{sae_id}/binding",
    response_model=SDNSAEBindingDTO,
)
def get_sae_binding_via_sdn_compat(
    simulation_id: int,
    sae_id: str,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SDNSAEBindingDTO:
    _validate_sim_header(simulation_id, x_simulation_id)
    with _sqlalchemy_uow_session() as (_, session):
        _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
        )
        entity = _require_owned_sae_in_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(simulation_id),
            sae_id=sae_id,
        )
        return _build_sae_binding_payload(session, sae_entity=entity)


@app.get(
    "/orch/api/sim/{simulation_id}/sdn/resolve-sae",
    response_model=SDNSAEBindingDTO,
)
def resolve_sae_via_sdn_compat(
    simulation_id: int,
    sae_id: str = Query(min_length=1),
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SDNSAEBindingDTO:
    return get_sae_binding_via_sdn_compat(
        simulation_id=simulation_id,
        sae_id=sae_id,
        x_user_id=x_user_id,
        x_simulation_id=x_simulation_id,
    )


@app.get(
    "/orch/admin/saes",
    response_model=list[SaeAdminDTO],
)
def list_admin_saes(
    simulation_id: Optional[int] = Query(default=None, ge=1),
    dkms_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> list[SaeAdminDTO]:
    from persistence.sqlalchemy.data import SAE as SAEEntity

    with _sqlalchemy_uow_session() as (_, session):
        resolved_dkms_id: Optional[int] = None
        if simulation_id is not None:
            _require_owned_simulation(session, user_id=x_user_id, simulation_id=int(simulation_id))
        if dkms_id is not None:
            if simulation_id is None:
                raise HTTPException(
                    status_code=400,
                    detail="simulation_id is required when dkms_id filter is provided",
                )
            resolved_dkms = _resolve_dkms_for_simulation(
                session,
                simulation_id=int(simulation_id),
                dkms_selector=int(dkms_id),
            )
            resolved_dkms_id = int(resolved_dkms.id)

        query = session.query(SAEEntity).filter(SAEEntity.owner_user_id == int(x_user_id))
        if simulation_id is not None:
            query = query.filter(SAEEntity.simulation_id == int(simulation_id))
        if resolved_dkms_id is not None:
            query = query.filter(SAEEntity.dkms_id == resolved_dkms_id)

        entities = query.order_by(SAEEntity.id.asc()).all()
        return [_sae_entity_to_dto(entity) for entity in entities]


@app.post(
    "/orch/admin/saes",
    response_model=SaeAdminDTO,
    status_code=status.HTTP_201_CREATED,
)
def create_admin_sae(
    payload: SaeAdminCreateRequest,
    x_user_id: int = Depends(_require_user_id),
) -> SaeAdminDTO:
    from persistence.sqlalchemy.data import SAE as SAEEntity
    from persistence.sqlalchemy.data import SaeStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        simulation_entity = _require_owned_simulation(
            session,
            user_id=x_user_id,
            simulation_id=int(payload.simulation_id),
        )
        resolved_dkms = _require_dkms_in_simulation(
            session,
            simulation_id=int(payload.simulation_id),
            dkms_id=int(payload.dkms_id),
        )
        resolved_dkms_id = int(resolved_dkms.id)

        existing = (
            session.query(SAEEntity)
            .filter(SAEEntity.simulation_id == int(payload.simulation_id))
            .filter(SAEEntity.sae_id == payload.sae_id)
            .one_or_none()
        )
        if existing is not None:
            if _simulation_is_running(simulation_entity) and getattr(existing, "dkms_id", None) is not None:
                _sync_sae_binding_to_sdn(
                    simulation_entity=simulation_entity,
                    sae_id=str(getattr(existing, "sae_id", "") or payload.sae_id),
                    dkms_id=int(existing.dkms_id),
                    session=session,
                )
            return _sae_entity_to_dto(existing)

        controller = _resolve_agent_controller_for_dkms(session, dkms_id=resolved_dkms_id)

        entity = SAEEntity(
            sae_id=payload.sae_id,
            display_name=payload.display_name or payload.sae_id,
            owner_user_id=int(x_user_id),
            simulation_id=int(payload.simulation_id),
            dkms_id=resolved_dkms_id,
            status=SaeStatusEnum.PENDING_CERT,
            sdn_id=int(controller.id_sdn) if controller and controller.id_sdn is not None else None,
            agent_dkms_id=int(controller.id) if controller else None,
            tls_id=None,
        )
        session.add(entity)
        try:
            session.flush()
        except IntegrityError:
            # Race: a concurrent retry (client 30s timeout → reintenta) insertó
            # el mismo (simulation_id, sae_id) entre nuestro existing-check y
            # el flush. Rollback y tratamos como idempotente.
            session.rollback()
            existing = (
                session.query(SAEEntity)
                .filter(SAEEntity.simulation_id == int(payload.simulation_id))
                .filter(SAEEntity.sae_id == payload.sae_id)
                .one_or_none()
            )
            if existing is not None:
                if _simulation_is_running(simulation_entity) and getattr(existing, "dkms_id", None) is not None:
                    _sync_sae_binding_to_sdn(
                        simulation_entity=simulation_entity,
                        sae_id=str(getattr(existing, "sae_id", "") or payload.sae_id),
                        dkms_id=int(existing.dkms_id),
                        session=session,
                    )
                return _sae_entity_to_dto(existing)
            raise
        if _simulation_is_running(simulation_entity):
            _sync_sae_binding_to_sdn(
                simulation_entity=simulation_entity,
                sae_id=str(entity.sae_id or payload.sae_id),
                dkms_id=resolved_dkms_id,
                session=session,
            )
        session.refresh(entity)
        uow.commit()
        return _sae_entity_to_dto(entity)


@app.post(
    "/orch/admin/saes/{sae_id}/csr",
    response_model=SaeIssueResponse,
)
def issue_admin_sae_from_csr(
    sae_id: str,
    payload: SaeIssueCSRRequest,
    simulation_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> SaeIssueResponse:
    from persistence.sqlalchemy.data import SaeStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        sae_entity = _require_owned_sae(
            session,
            user_id=x_user_id,
            sae_id=sae_id,
            simulation_id=simulation_id,
        )
        runtime_ca_cert_pem, runtime_ca_key_pem = _runtime_ca_material_for_sae(sae_entity=sae_entity)
        issued = sign_sae_csr(
            sae_id=sae_entity.sae_id or sae_id,
            csr_pem=payload.csr_pem,
            days_valid=int(payload.days_valid),
            ca_certificate_pem=runtime_ca_cert_pem,
            ca_private_key_pem=runtime_ca_key_pem,
        )
        _upsert_sae_tls_bundle(
            session,
            sae_entity=sae_entity,
            certificate_pem=issued.certificate_pem,
            ca_chain_pem=issued.ca_chain_pem,
            private_key_pem=issued.private_key_pem,
        )
        sae_entity.status = SaeStatusEnum.ACTIVE
        sae_entity.cert_serial = issued.serial_hex
        sae_entity.cert_fingerprint = issued.fingerprint_sha256
        sae_entity.cert_subject = issued.subject_rfc4514
        sae_entity.cert_not_before = issued.not_before
        sae_entity.cert_not_after = issued.not_after
        sae_entity.revoked_at = None
        sae_entity.revocation_reason = None
        sae_entity.updated_at = datetime.now(timezone.utc)
        session.flush()
        session.refresh(sae_entity)
        uow.commit()
        return SaeIssueResponse(
            sae=_sae_entity_to_dto(sae_entity),
            certificate_pem=issued.certificate_pem,
            ca_chain_pem=issued.ca_chain_pem,
            private_key_pem=issued.private_key_pem,
            bundle_pkcs12_base64=None,
        )


@app.post(
    "/orch/admin/saes/{sae_id}/issue",
    response_model=SaeIssueResponse,
)
def issue_admin_sae_server_side(
    sae_id: str,
    payload: SaeIssueRequest,
    simulation_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> SaeIssueResponse:
    from persistence.sqlalchemy.data import SaeStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        sae_entity = _require_owned_sae(
            session,
            user_id=x_user_id,
            sae_id=sae_id,
            simulation_id=simulation_id,
        )
        runtime_ca_cert_pem, runtime_ca_key_pem = _runtime_ca_material_for_sae(sae_entity=sae_entity)
        issued = issue_sae_certificate(
            sae_id=sae_entity.sae_id or sae_id,
            key_type=payload.key_type,
            days_valid=int(payload.days_valid),
            ca_certificate_pem=runtime_ca_cert_pem,
            ca_private_key_pem=runtime_ca_key_pem,
        )
        _upsert_sae_tls_bundle(
            session,
            sae_entity=sae_entity,
            certificate_pem=issued.certificate_pem,
            ca_chain_pem=issued.ca_chain_pem,
            private_key_pem=issued.private_key_pem,
        )

        sae_entity.status = SaeStatusEnum.ACTIVE
        sae_entity.cert_serial = issued.serial_hex
        sae_entity.cert_fingerprint = issued.fingerprint_sha256
        sae_entity.cert_subject = issued.subject_rfc4514
        sae_entity.cert_not_before = issued.not_before
        sae_entity.cert_not_after = issued.not_after
        sae_entity.revoked_at = None
        sae_entity.revocation_reason = None
        sae_entity.updated_at = datetime.now(timezone.utc)
        session.flush()
        session.refresh(sae_entity)
        uow.commit()

        bundle_pkcs12 = None
        if payload.bundle_format == "pkcs12":
            if not issued.private_key_pem:
                raise HTTPException(status_code=409, detail="No private key available for PKCS12 bundle")
            bundle_pkcs12 = bundle_to_pkcs12_base64(
                certificate_pem=issued.certificate_pem,
                private_key_pem=issued.private_key_pem,
                ca_chain_pem=issued.ca_chain_pem,
                password=payload.pkcs12_password,
            )

        return SaeIssueResponse(
            sae=_sae_entity_to_dto(sae_entity),
            certificate_pem=issued.certificate_pem,
            ca_chain_pem=issued.ca_chain_pem,
            private_key_pem=issued.private_key_pem,
            bundle_pkcs12_base64=bundle_pkcs12,
        )


@app.get(
    "/orch/admin/saes/{sae_id}/bundle",
    response_model=SaeBundleResponse,
)
def get_admin_sae_bundle(
    sae_id: str,
    format: Literal["pem", "pkcs12"] = Query(default="pem"),
    pkcs12_password: Optional[str] = Query(default=None),
    simulation_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> SaeBundleResponse:
    from persistence.sqlalchemy.data import DataFile as DataFileEntity
    from persistence.sqlalchemy.data import TLSConfigSAE as TLSConfigSAEEntity

    with _sqlalchemy_uow_session() as (_, session):
        sae_entity = _require_owned_sae(
            session,
            user_id=x_user_id,
            sae_id=sae_id,
            simulation_id=simulation_id,
        )
        if sae_entity.tls_id is None:
            raise HTTPException(status_code=404, detail="SAE has no certificate bundle")

        tls_entity = session.get(TLSConfigSAEEntity, int(sae_entity.tls_id))
        if tls_entity is None:
            raise HTTPException(status_code=404, detail="TLS bundle not found for SAE")

        cert_file = session.get(DataFileEntity, int(tls_entity.cert_id))
        key_file = session.get(DataFileEntity, int(tls_entity.key_id))
        ca_file = session.get(DataFileEntity, int(tls_entity.ca_certs_id))

        cert_pem = str(getattr(cert_file, "data", "") or "")
        key_pem = str(getattr(key_file, "data", "") or "")
        ca_pem = str(getattr(ca_file, "data", "") or "")

        if not cert_pem or not ca_pem:
            raise HTTPException(status_code=409, detail="Stored TLS bundle is incomplete")

        if format == "pkcs12":
            if not key_pem:
                raise HTTPException(
                    status_code=409,
                    detail="PKCS12 bundle is not available (private key not stored server-side)",
                )
            bundle_pkcs12 = bundle_to_pkcs12_base64(
                certificate_pem=cert_pem,
                private_key_pem=key_pem,
                ca_chain_pem=ca_pem,
                password=pkcs12_password,
            )
            return SaeBundleResponse(
                sae=_sae_entity_to_dto(sae_entity),
                format="pkcs12",
                bundle_pkcs12_base64=bundle_pkcs12,
            )

        return SaeBundleResponse(
            sae=_sae_entity_to_dto(sae_entity),
            format="pem",
            certificate_pem=cert_pem,
            private_key_pem=key_pem or None,
            ca_chain_pem=ca_pem,
        )


@app.post(
    "/orch/admin/saes/{sae_id}/revoke",
    response_model=SaeAdminDTO,
)
def revoke_admin_sae(
    sae_id: str,
    payload: SaeRevokeRequest,
    simulation_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> SaeAdminDTO:
    from persistence.sqlalchemy.data import SaeStatusEnum

    with _sqlalchemy_uow_session() as (uow, session):
        sae_entity = _require_owned_sae(
            session,
            user_id=x_user_id,
            sae_id=sae_id,
            simulation_id=simulation_id,
        )
        sae_entity.status = SaeStatusEnum.REVOKED
        sae_entity.revoked_at = datetime.now(timezone.utc)
        sae_entity.revocation_reason = payload.reason or "revoked"
        sae_entity.updated_at = datetime.now(timezone.utc)
        session.flush()
        session.refresh(sae_entity)
        uow.commit()
        return _sae_entity_to_dto(sae_entity)


@app.delete(
    "/orch/admin/saes/{sae_id}",
    response_model=SaeDeleteResponse,
)
def delete_admin_sae(
    sae_id: str,
    simulation_id: Optional[int] = Query(default=None, ge=1),
    x_user_id: int = Depends(_require_user_id),
) -> SaeDeleteResponse:
    with _sqlalchemy_uow_session() as (uow, session):
        sae_entity = _require_owned_sae(
            session,
            user_id=x_user_id,
            sae_id=sae_id,
            simulation_id=simulation_id,
        )
        resolved_sae_id = str(getattr(sae_entity, "sae_id", "") or sae_id).strip() or sae_id
        # Best-effort SDN unbind before dropping the DB row — otherwise the
        # SDN keeps a stale SAE→DKMS entry that blocks re-registration with
        # the same sae_id on subsequent runs.
        sim_pk = getattr(sae_entity, "simulation_id", None)
        if sim_pk is not None:
            try:
                simulation_entity = _require_owned_simulation(
                    session,
                    user_id=x_user_id,
                    simulation_id=int(sim_pk),
                )
                if _simulation_is_running(simulation_entity):
                    _delete_sae_binding_from_sdn(
                        simulation_entity=simulation_entity,
                        sae_id=resolved_sae_id,
                    )
            except HTTPException as exc:
                # 503 "SDN backend unavailable" under heavy load is tolerable
                # on delete (caller re-deploys the sim and the SDN starts
                # empty). Log and continue so the DB row still gets removed.
                if exc.status_code != status.HTTP_503_SERVICE_UNAVAILABLE:
                    raise
        session.delete(sae_entity)
        session.flush()
        uow.commit()
        # Evita respuesta ambigua en proxies/clients que esperan contenido JSON.
        return SaeDeleteResponse(status="deleted", sae_id=resolved_sae_id)


@app.get(
    "/orch/web/simulations",
    response_model=list[WebSimulationSummaryDTO],
)
def web_list_simulations(
    x_user_id: int = Depends(_require_user_id),
) -> list[WebSimulationSummaryDTO]:
    orchestator = _build_orchestator()
    try:
        simulations = orchestator.get_simulations(x_user_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    result: list[WebSimulationSummaryDTO] = []
    for simulation in simulations:
        dto = _simulation_to_web_dto(simulation)
        result.append(_summary_from_web_dto(dto))
    return result


@app.post(
    "/orch/web/simulations",
    response_model=WebSimulationDTO,
    status_code=status.HTTP_201_CREATED,
)
def web_create_simulation(
    payload: WebSimulationUpsertRequest,
    x_user_id: int = Depends(_require_user_id),
) -> WebSimulationDTO:
    simulation_model = _build_model_simulation_from_web(x_user_id, payload)
    orchestator = _build_orchestator()

    try:
        created = orchestator.create_simulation(simulation_model)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    topology_json = _topology_json_from_request(payload)
    _persist_editor_topology(
        int(created.id),
        topology_json,
        payload.name.strip(),
        payload.description,
    )

    refreshed = orchestator.get_simulation_id(x_user_id, int(created.id))
    return _simulation_to_web_dto(refreshed)


@app.get(
    "/orch/web/simulations/{simulation_id}",
    response_model=WebSimulationDTO,
)
def web_get_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> WebSimulationDTO:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()
    try:
        simulation = orchestator.get_simulation_id(x_user_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return _simulation_to_web_dto(simulation)


@app.patch(
    "/orch/web/simulations/{simulation_id}",
    response_model=WebSimulationDTO,
)
def web_patch_simulation(
    simulation_id: int,
    payload: WebSimulationUpsertRequest,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> WebSimulationDTO:
    _validate_sim_header(simulation_id, x_simulation_id)
    try:
        updated = _replace_simulation_graph(x_user_id, simulation_id, payload)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc
    return _simulation_to_web_dto(updated)


@app.delete(
    "/orch/web/simulations/{simulation_id}",
    response_model=SimulationActionResponse,
)
def web_delete_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    return delete_simulation(simulation_id, x_user_id, x_simulation_id)


@app.post(
    "/orch/web/simulations/{simulation_id}/run",
    response_model=WebSimulationRunDTO,
    status_code=status.HTTP_201_CREATED,
)
def web_run_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> WebSimulationRunDTO:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()

    try:
        simulation = orchestator.get_simulation_id(x_user_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    if simulation.status == SimulationStatus.RUNNING:
        raise HTTPException(
            status_code=409,
            detail="Simulation infrastructure is already running",
        )

    topology_raw = getattr(simulation, "editor_topology_json", None)
    if topology_raw:
        try:
            topology = json.loads(topology_raw)
            nodes = [WebNodeInput.model_validate(item) for item in topology.get("nodes", [])]
            links = [WebLinkInput.model_validate(item) for item in topology.get("links", [])]
            if not _is_connected(nodes, links):
                raise HTTPException(
                    status_code=400,
                    detail="Topology is not connected; cannot run simulation",
                )
        except HTTPException:
            raise
        except Exception:  # noqa: BLE001
            pass

    now = datetime.now(timezone.utc)
    run = _create_run_record(
        simulation_id=simulation_id,
        status_text="QUEUED",
        message="Queued to deploy infrastructure",
        queued_at=now,
    )

    _update_run_record(
        run.id,
        "RUNNING",
        message="Deploying infrastructure",
        started_at=datetime.now(timezone.utc),
    )

    try:
        orchestator.run_simulation(str(x_user_id), simulation_id)
    except ValueError as exc:
        _update_run_record(
            run.id,
            "FAILED",
            message=str(exc),
            finished_at=datetime.now(timezone.utc),
        )
        raise _value_error_to_http(exc) from exc
    except Exception as exc:  # noqa: BLE001
        _update_run_record(
            run.id,
            "FAILED",
            message=str(exc),
            finished_at=datetime.now(timezone.utc),
        )
        raise HTTPException(status_code=500, detail="Failed to run simulation") from exc

    _update_run_record(run.id, "RUNNING", message="Infrastructure running")

    return _list_run_records(simulation_id=simulation_id)[0]


@app.post(
    "/orch/web/simulations/{simulation_id}/stop",
    response_model=SimulationActionResponse,
)
def web_stop_simulation(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()

    try:
        simulation = orchestator.get_simulation_id(x_user_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    if simulation.status != SimulationStatus.RUNNING:
        raise HTTPException(
            status_code=409,
            detail="Simulation infrastructure is not running",
        )

    try:
        orchestator.stop_simulation(str(x_user_id), simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    runs = _list_run_records(simulation_id)
    if runs and runs[0].status in {"QUEUED", "RUNNING"}:
        _update_run_record(
            runs[0].id,
            "DONE",
            message="Simulation stopped",
            finished_at=datetime.now(timezone.utc),
        )
    else:
        _create_run_record(
            simulation_id=simulation_id,
            status_text="DONE",
            message="Simulation stopped",
            finished_at=datetime.now(timezone.utc),
        )

    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action="stop",
    )


@app.post(
    "/orch/web/simulations/{simulation_id}/dkms/{dkms_id}/stop",
    response_model=SimulationActionResponse,
)
def web_stop_dkms_in_simulation(
    simulation_id: int,
    dkms_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()

    try:
        orchestator.stop_dkms(str(x_user_id), simulation_id, dkms_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action=f"stop-dkms-{dkms_id}",
    )


@app.post(
    "/orch/web/simulations/{simulation_id}/dkms/{dkms_id}/start",
    response_model=SimulationActionResponse,
)
def web_start_dkms_in_simulation(
    simulation_id: int,
    dkms_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> SimulationActionResponse:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()

    try:
        orchestator.start_dkms(str(x_user_id), simulation_id, dkms_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    return SimulationActionResponse(
        status="ok",
        simulation_id=simulation_id,
        action=f"start-dkms-{dkms_id}",
    )


@app.get(
    "/orch/web/simulations/{simulation_id}/runs",
    response_model=list[WebSimulationRunDTO],
)
def web_list_simulation_runs(
    simulation_id: int,
    x_user_id: int = Depends(_require_user_id),
    x_simulation_id: Optional[str] = Header(default=None, alias="X-Simulation-Id"),
) -> list[WebSimulationRunDTO]:
    _validate_sim_header(simulation_id, x_simulation_id)
    orchestator = _build_orchestator()

    try:
        orchestator.get_simulation_id(x_user_id, simulation_id)
    except ValueError as exc:
        raise _value_error_to_http(exc) from exc

    return _list_run_records(simulation_id)


if __name__ == "__main__":
    import uvicorn

    host = os.getenv("ORCHESTATOR_HOST", "0.0.0.0")
    port = int(os.getenv("ORCHESTATOR_PORT", "8080"))
    uvicorn.run(app, host=host, port=port)
