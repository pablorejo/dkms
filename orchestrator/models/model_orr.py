from __future__ import annotations

from typing import Optional

from .model import Model
from .model_host import ModelHost
from .model_qkc import ModelQKC


class ModelORR(Model):
    """Modelo pydantic para la tabla orr."""

    id: Optional[int] = None
    id_host: Optional[int] = None
    qkc_id: int
    host: Optional[ModelHost] = None
    qkc: Optional[ModelQKC] = None
