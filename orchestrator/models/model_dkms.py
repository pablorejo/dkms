from __future__ import annotations

from typing import List, Optional

from pydantic import Field

from .enums import CipherDKMS, TLSVersion
from .model_agent_controller import ModelAgentController
from .model_file_data import ModelFile
from .model_host import ModelHost
from .model_orr import ModelORR
from .model import Model


class TLSConfigDKMS(Model):
    """Configuración TLS asociada a un DKMS."""

    id: Optional[int] = None
    cert: ModelFile
    key: ModelFile
    ca_cert: ModelFile
    require_client_cert: bool
    version: TLSVersion
    ciphers: List[CipherDKMS]

class ModelDKMS(Model):
    """Modelo pydantic que representa la tabla dkms."""

    id: Optional[int] = None
    id_host: Optional[int] = None
    orr_id: int
    tls_id: Optional[int] = None
    host: Optional[ModelHost] = None
    orr: Optional[ModelORR] = None
    tls: Optional[TLSConfigDKMS] = None
    # agent_controllers: List[ModelAgentController] = Field(default_factory=list)


    @staticmethod
    def model_from_json_file(path):
        import json

        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        # Mejorado: usar el método recomendado model_validate en lugar de parse_obj
        return ModelDKMS.model_validate(data)
        
