from __future__ import annotations

from typing import Optional, TYPE_CHECKING

from .model import Model
from .model_host import ModelHost

if TYPE_CHECKING:
    from .model_dkms import ModelDKMS
    from .model_sdn import ModelSDN


class ModelAgentController(Model):
    """Modelo pydantic para la tabla agent_controller."""

    id: Optional[int] = None
    id_dkms: int
    id_sdn: Optional[int] = None
    id_host: Optional[int] = None
    dkms: Optional["ModelDKMS"] = None
    sdn: Optional["ModelSDN"] = None
    host: Optional[ModelHost] = None
