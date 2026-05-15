"""baseline: esquema actual ya aplicado vía SQL legacy en orchestrator/persistence/sqlalchemy/migrations/

Las migraciones SQL crudas (20260226 a 20260417) construyeron el esquema
hasta este punto. Esta revision queda como punto de partida para los
próximos cambios gestionados por Alembic. Si arrancás contra una DB
vacía, primero ejecutá los .sql en orden y luego ``alembic stamp head``.

Revision ID: 20260515_0000
Revises:
Create Date: 2026-05-15
"""
from __future__ import annotations

from typing import Sequence, Union


revision: str = "20260515_0000"
down_revision: Union[str, Sequence[str], None] = None
branch_labels: Union[str, Sequence[str], None] = None
depends_on: Union[str, Sequence[str], None] = None


def upgrade() -> None:
    pass


def downgrade() -> None:
    pass
