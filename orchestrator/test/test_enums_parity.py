"""Verifica que cada Pydantic enum en models/enums.py tenga la misma
correspondencia name->value que su contraparte SQLAlchemy en
persistence/sqlalchemy/data.py.

Pensado para evitar incidentes como el HTTPType invertido (values
swapped) que pasaba sin detectar porque ningún test cruzaba ambas
familias.

Ejecutar con: ``pytest orchestrator/test/test_enums_parity.py``
"""
from __future__ import annotations

import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from models.enums import (  # noqa: E402
    ChannelType,
    CipherDKMS,
    ETSIType,
    HTTPType,
    SaeStatus,
    SimulationStatus,
    TLSVersion,
)
from persistence.sqlalchemy.data import (  # noqa: E402
    ChannelTypeEnum,
    CipherDKMSEnum,
    ETSIEnum,
    HTTPTypeEnum,
    SaeStatusEnum,
    SimulationStatusEnum,
    TLSVersionEnum,
)

ENUM_PAIRS = [
    ("ETSIType", ETSIType, ETSIEnum),
    ("ChannelType", ChannelType, ChannelTypeEnum),
    ("CipherDKMS", CipherDKMS, CipherDKMSEnum),
    ("TLSVersion", TLSVersion, TLSVersionEnum),
    ("SimulationStatus", SimulationStatus, SimulationStatusEnum),
    ("HTTPType", HTTPType, HTTPTypeEnum),
    ("SaeStatus", SaeStatus, SaeStatusEnum),
]


@pytest.mark.parametrize("name,pydantic_enum,orm_enum", ENUM_PAIRS)
def test_enum_parity(name, pydantic_enum, orm_enum):
    pydantic_pairs = {member.name: member.value for member in pydantic_enum}
    orm_pairs = {member.name: member.value for member in orm_enum}
    assert pydantic_pairs == orm_pairs, (
        f"{name} drift: pydantic={pydantic_pairs} orm={orm_pairs}"
    )
