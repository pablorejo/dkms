from __future__ import annotations

from datetime import datetime
from typing import Optional

from .enums import SaeStatus
from .model import Model
from .model_agent_controller import ModelAgentController
from .model_file_data import ModelFile
from .model_sdn import ModelSDN


class TLSConfigSAE(Model):
    """Configuración TLS utilizada por un SAE."""

    id: Optional[int] = None
    cert: ModelFile
    key: ModelFile
    ca_certs: ModelFile
    use_client_cert: bool

class DKMS_Target(Model):
    """Compatibilidad temporal para describir el destino DKMS."""

    ip: str
    port: int

class ModelSAE(Model):
    """Modelo Pydantic que representa un SAE persistido."""

    id: Optional[int] = None
    sae_id: Optional[str] = None
    display_name: Optional[str] = None
    sdn_id: Optional[int] = None
    tls_id: Optional[int] = None
    agent_dkms_id: Optional[int] = None
    owner_user_id: Optional[int] = None
    simulation_id: Optional[int] = None
    dkms_id: Optional[int] = None
    status: SaeStatus = SaeStatus.PENDING_CERT
    cert_serial: Optional[str] = None
    cert_fingerprint: Optional[str] = None
    cert_subject: Optional[str] = None
    cert_not_before: Optional[datetime] = None
    cert_not_after: Optional[datetime] = None
    revoked_at: Optional[datetime] = None
    revocation_reason: Optional[str] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None
    sdn: Optional[ModelSDN] = None
    tls: Optional[TLSConfigSAE] = None
    agent_controller: Optional[ModelAgentController] = None
    dkms_target: Optional[DKMS_Target] = None  # Campo legacy mientras se completa la migración
