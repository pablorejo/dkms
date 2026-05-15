from __future__ import annotations

from datetime import datetime
from typing import List, Optional

from models.model_dkms import ModelDKMS
from models.model_sdn import ModelSDN

from .enums import SimulationStatus
from .model import Model
from .model_user import ModelUser


class ModelSimulation(Model):
    """Modelo Pydantic que representa una simulación."""

    id: Optional[int] = None
    id_user: int
    name: str
    description: Optional[str] = None
    editor_topology_json: Optional[str] = None
    created_at: Optional[datetime] = None
    updated_at: Optional[datetime] = None
    start_time: Optional[datetime] = None
    end_time: Optional[datetime] = None
    status: Optional[SimulationStatus] = None
    user: Optional[ModelUser] = None

    list_dkms: List[ModelDKMS]
    sdn: ModelSDN

    @staticmethod
    def model_from_json_file(path):
        import json

        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        # Mejorado: usar el método recomendado model_validate en lugar de parse_obj
        return ModelSimulation.model_validate(data)
