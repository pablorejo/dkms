from __future__ import annotations

from typing import List, Optional

from pydantic import AliasChoices, Field

from .enums import ETSIType, ChannelType
from .model_file_data import ModelFile
from .model_host import ModelHost
from .model import Model

class TokenBucketConfig(Model):
    beta: Optional[float] = None
    gamma: Optional[float] = None
    delta_up: Optional[float] = None
    delta_down: Optional[float] = None
    T_obs: Optional[float] = None
    alpha0: Optional[float] = None
    alpha_min: Optional[float] = None
    tau: Optional[float] = None
    B_min: Optional[int] = None
    B_max: Optional[int] = None
    R_max: Optional[int] = None
    initial_tokens: Optional[int] = None
    status_timeout: Optional[float] = None
    verify: Optional[bool] = None

class Channel(Model):
    """Caracteristicas del canal"""
    type_channel: ChannelType
    distance: int # Kilometers
    quditto_max_buffer_size: int = Field(default=100, ge=1)
    quditto_rate_r0: float = Field(default=2000.0, gt=0)
    quditto_rate_alpha: float = Field(default=0.2, ge=0)


class KMEConfig(Model):
    """Configuración de un enlace KME asociado a un QKC."""

    id: Optional[int] = None
    local_qkc_id: int  # ID of the qkc that uses this kme
    local_qkc_ip: str  # Ip of the qkc

    neighbor_qkc_id: int  # ID of the qkc that this qkc is connected with via this kme
    neighbor_qkc_ip: str
    neighbor_qkc_port: int  # port of the neighbor qkc

    neighbor_qkd_id: str
    local_qkd_id: str

    local_url_node_qkd: str

    etsi: ETSIType
    cert: ModelFile
    key: ModelFile
    token_bucket: Optional[TokenBucketConfig] = None
    channel: Channel = Field(
        default_factory=lambda: Channel(
            type_channel=ChannelType.QKD,
            distance=0,
            quditto_max_buffer_size=100,
            quditto_rate_r0=2000.0,
            quditto_rate_alpha=0.2,
        )
    )
    pqc_simulation: bool = Field(
        default=False,
        validation_alias=AliasChoices("pqc_simulation", "pqc-simulation", "pqc_simultaion"),
    )
    hybrid_enabled: bool = Field(
        default=False,
        validation_alias=AliasChoices("hybrid_enabled", "hybrid-enabled"),
    )
    pqc_kme_port: int = Field(
        default=6000,
        validation_alias=AliasChoices("pqc_kme_port", "pqc-kme-port"),
    )

class ModelQKC(Model):
    """Modelo pydantic de la tabla qkc."""

    id: Optional[int] = None
    id_host: Optional[int] = None
    kme_host: Optional[str] = None
    host: Optional[ModelHost] = None
    kmes: List[KMEConfig] = Field(default_factory=list)
