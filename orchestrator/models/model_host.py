from __future__ import annotations

from typing import Optional, TYPE_CHECKING

from .model import Model

if TYPE_CHECKING:
    # Imported only for type checking to avoid circular imports at runtime
    from .model_simulation import ModelSimulation


class ModelHost(Model):
    """Modelo pydantic para la tabla host."""

    id: Optional[int] = None
    id_simulation: int
    ip: str
    port: int
    simulation: Optional[ModelSimulation] = None

    def get_host(self):
        return f"{self.ip}:{self.port}"
