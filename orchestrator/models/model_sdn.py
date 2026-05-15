from __future__ import annotations

from typing import Optional

from .enums import HTTPType
from .model import Model
from .model_host import ModelHost


class ModelSDN(Model):
    """Modelo Pydantic para representar un SDN persistido."""

    id: Optional[int] = None
    id_host: Optional[int] = None
    host: Optional[ModelHost] = None

    type_http : HTTPType

    def get_host(self) -> str:
        return f"{self.type_http.value}://{self.host.get_host()}"


    @staticmethod
    def model_from_json_file(path):
        import json

        with open(path, "r", encoding="utf-8") as f:
            data = json.load(f)
        # Mejorado: usar el método recomendado model_validate en lugar de parse_obj
        return ModelSDN.model_validate(data)
