from __future__ import annotations

from typing import Optional

from models.model_dkms import ModelDKMS

from .model import Model

class ModelUser(Model):
    """Modelo pydantic para la tabla user."""

    id: Optional[int] = None
    username: str
    email: str
    password_hash: str
    is_active: bool = False

