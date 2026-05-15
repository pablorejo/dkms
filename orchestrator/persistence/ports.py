from __future__ import annotations

from dataclasses import dataclass
from typing import Optional, Protocol

from models import (
    ModelAgentController,
    ModelDKMS,
    ModelORR,
    ModelQKC,
    ModelSAE,
    ModelSimulation,
    ModelUser,
)


class SimulationRepo(Protocol):
    """Repositorio de simulaciones."""

    def save(self, model: ModelSimulation) -> ModelSimulation:
        ...

    def get(self, simulation_id: int) -> Optional[ModelSimulation]:
        ...

    def delete(self, simulation_id: int) -> bool:
        ...

    def update(self, model: ModelSimulation) -> ModelSimulation:
        ...


class DKMSRepo(Protocol):
    """Repositorio de DKMS."""

    def save(self, model: ModelDKMS) -> ModelDKMS:
        ...

    def get(self, dkms_id: int) -> Optional[ModelDKMS]:
        ...

    def delete(self, dkms_id: int) -> bool:
        ...

    def update(self, model: ModelDKMS) -> ModelDKMS:
        ...


class QKCRepo(Protocol):
    """Repositorio de QKC."""

    def save(self, model: ModelQKC) -> ModelQKC:
        ...

    def get(self, qkc_id: int) -> Optional[ModelQKC]:
        ...

    def delete(self, qkc_id: int) -> bool:
        ...

    def update(self, model: ModelQKC) -> ModelQKC:
        ...


class ORRRepo(Protocol):
    """Repositorio de ORR."""

    def save(self, model: ModelORR) -> ModelORR:
        ...

    def get(self, orr_id: int) -> Optional[ModelORR]:
        ...

    def delete(self, orr_id: int) -> bool:
        ...

    def update(self, model: ModelORR) -> ModelORR:
        ...


class UserRepo(Protocol):
    """Repositorio de usuarios."""

    def save(self, model: ModelUser) -> ModelUser:
        ...

    def get(self, user_id: int) -> Optional[ModelUser]:
        ...

    def get_by_username(self, username: str) -> Optional[ModelUser]:
        ...

    def get_by_email(self, email: str) -> Optional[ModelUser]:
        ...

    def delete(self, user_id: int) -> bool:
        ...

    def update(self, model: ModelUser) -> ModelUser:
        ...


class SAERepo(Protocol):
    """Repositorio de SAEs gestionados."""

    def save(self, model: ModelSAE) -> ModelSAE:
        ...

    def get(self, sae_row_id: int) -> Optional[ModelSAE]:
        ...

    def get_by_sae_id(
        self,
        sae_id: str,
        simulation_id: Optional[int] = None,
    ) -> Optional[ModelSAE]:
        ...

    def list_by_simulation(self, simulation_id: int) -> list[ModelSAE]:
        ...

    def list_by_dkms(self, dkms_id: int) -> list[ModelSAE]:
        ...

    def find_active_by_fingerprint(self, fingerprint: str) -> Optional[ModelSAE]:
        ...

    def delete(self, sae_row_id: int) -> bool:
        ...

    def update(self, model: ModelSAE) -> ModelSAE:
        ...


class AgentControllerRepo(Protocol):
    """Consultas sobre agent-controllers asociados a nodos."""

    def save(self, model: ModelAgentController) -> ModelAgentController:
        ...

    def get(self, controller_id: int) -> Optional[ModelAgentController]:
        ...

    def delete(self, controller_id: int) -> bool:
        ...

    def update(self, model: ModelAgentController) -> ModelAgentController:
        ...

    def by_dkms_id(self, dkms_id: int) -> list[ModelAgentController]:
        ...

    def by_orr_id(self, orr_id: int) -> list[ModelAgentController]:
        ...

    def by_qkc_id(self, qkc_id: int) -> list[ModelAgentController]:
        ...


@dataclass(frozen=True)
class Repositories:
    """Agrupa todos los repositorios disponibles en la UoW."""

    simulations: SimulationRepo
    dkms: DKMSRepo
    qkcs: QKCRepo
    orrs: ORRRepo
    users: UserRepo
    saes: SAERepo
    agent_controllers: AgentControllerRepo


class UnitOfWork(Protocol):
    """Contrato base para UoW que maneja transacciones y repositorios."""

    repos: Repositories

    def __enter__(self) -> UnitOfWork:
        ...

    def __exit__(self, exc_type, exc, tb) -> None:
        ...

    def commit(self) -> None:
        ...

    def rollback(self) -> None:
        ...
