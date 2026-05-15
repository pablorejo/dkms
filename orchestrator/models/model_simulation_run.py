from __future__ import annotations

from datetime import datetime
from typing import Optional

from .model import Model


class ModelSimulationRun(Model):
    id: Optional[int] = None
    simulation_id: int
    status: str
    message: Optional[str] = None
    queued_at: Optional[datetime] = None
    started_at: Optional[datetime] = None
    finished_at: Optional[datetime] = None
    created_at: Optional[datetime] = None
