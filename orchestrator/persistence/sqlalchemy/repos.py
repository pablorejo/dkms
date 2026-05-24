from __future__ import annotations

from typing import Optional

from sqlalchemy.orm import Session

from models import (
    ModelAgentController,
    ModelDKMS,
    ModelORR,
    ModelQKC,
    ModelSAE,
    ModelSimulation,
    ModelUser,
)

from .data import AgentController, DKMS, Host, ORR, QKC, SAE, SaeStatusEnum, Simulation, User
from .mappers import Entity2Model, Model2Entity


class _RepoBase:
    def __init__(self, session: Session) -> None:
        self._session = session

    def _require_existing(self, entity_cls, entity_id: Optional[int]) -> None:
        if entity_id is None:
            raise ValueError(f"El identificador es obligatorio para actualizar {entity_cls.__name__}")
        exists = self._session.get(entity_cls, entity_id)
        if exists is None:
            raise ValueError(f"{entity_cls.__name__} con id {entity_id} no existe")

    def _resolve_id_simulation(self, model) -> int:
        """Devuelve `id_simulation` para mapper KME/QKC/ORR/DKMS.

        Busca, en orden: model.host.id_simulation, lookup en BD por
        model.id_host. Necesario tras añadir id_simulation FK en kme/qkc
        (project_bd_orphan_kmes_inflate_sdn).
        """
        host = getattr(model, "host", None)
        if host is not None:
            sim = getattr(host, "id_simulation", None)
            if sim is not None:
                return int(sim)
        id_host = getattr(model, "id_host", None)
        if id_host is None:
            raise ValueError(
                "No se puede resolver id_simulation sin host ni id_host"
            )
        host_entity = self._session.get(Host, id_host)
        if host_entity is None:
            raise ValueError(f"Host con id {id_host} no existe")
        return int(host_entity.id_simulation)

    def _ensure_host(self, model, allow_none: bool = False):
        host = getattr(model, "host", None)
        id_host = getattr(model, "id_host", None)
        if host is None:
            if id_host is None:
                if allow_none:
                    return model
                raise ValueError("El id_host es obligatorio si no se provee host")
            if self._session.get(Host, id_host) is None:
                raise ValueError(f"Host con id {id_host} no existe")
            return model

        if id_host is not None:
            existing = self._session.get(Host, id_host)
            if existing is not None:
                host = host.model_copy(update={"id": existing.id})
                return model.model_copy(update={"id_host": existing.id, "host": host})

        if host.id is not None:
            existing = self._session.get(Host, host.id)
            if existing is not None:
                host = host.model_copy(update={"id": existing.id})
                return model.model_copy(update={"id_host": existing.id, "host": host})

        existing = (
            self._session.query(Host)
            .filter(
                Host.id_simulation == host.id_simulation,
                Host.ip == host.ip,
                Host.port == host.port,
            )
            .one_or_none()
        )
        if existing is not None:
            host = host.model_copy(update={"id": existing.id})
            return model.model_copy(update={"id_host": existing.id, "host": host})

        host_entity = Model2Entity.host(host.model_copy(update={"id": None}))
        merged = self._session.merge(host_entity)
        self._session.flush()
        self._session.refresh(merged)
        host = host.model_copy(update={"id": merged.id})
        return model.model_copy(update={"id_host": merged.id, "host": host})


class SimulationRepoSA(_RepoBase):
    def save(self, model: ModelSimulation) -> ModelSimulation:
        entity = Model2Entity.simulation(model)
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        return Entity2Model.simulation(merged)

    def get(self, simulation_id: int) -> Optional[ModelSimulation]:
        entity = self._session.get(Simulation, simulation_id)
        return Entity2Model.simulation(entity) if entity else None

    def delete(self, simulation_id: int) -> bool:
        entity = self._session.get(Simulation, simulation_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelSimulation) -> ModelSimulation:
        self._require_existing(Simulation, model.id)
        return self.save(model)


class DKMSRepoSA(_RepoBase):
    def save(self, model: ModelDKMS) -> ModelDKMS:
        model = self._ensure_host(model)
        entity = Model2Entity.dkms(model, id_simulation=self._resolve_id_simulation(model))
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        saved = Entity2Model.dkms(merged)
        if saved is None:
            raise ValueError("No se pudo convertir el DKMS persistido")
        return saved

    def get(self, dkms_id: int) -> Optional[ModelDKMS]:
        entity = self._session.get(DKMS, dkms_id)
        return Entity2Model.dkms(entity)

    def delete(self, dkms_id: int) -> bool:
        entity = self._session.get(DKMS, dkms_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelDKMS) -> ModelDKMS:
        self._require_existing(DKMS, model.id)
        return self.save(model)


class QKCRepoSA(_RepoBase):
    def save(self, model: ModelQKC) -> ModelQKC:
        model = self._ensure_host(model)
        entity = Model2Entity.qkc(model, id_simulation=self._resolve_id_simulation(model))
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        saved = Entity2Model.qkc(merged)
        if saved is None:
            raise ValueError("No se pudo convertir el QKC persistido")
        return saved

    def get(self, qkc_id: int) -> Optional[ModelQKC]:
        entity = self._session.get(QKC, qkc_id)
        return Entity2Model.qkc(entity)

    def delete(self, qkc_id: int) -> bool:
        entity = self._session.get(QKC, qkc_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelQKC) -> ModelQKC:
        self._require_existing(QKC, model.id)
        return self.save(model)


class ORRRepoSA(_RepoBase):
    def save(self, model: ModelORR) -> ModelORR:
        model = self._ensure_host(model)
        entity = Model2Entity.orr(model, id_simulation=self._resolve_id_simulation(model))
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        saved = Entity2Model.orr(merged)
        if saved is None:
            raise ValueError("No se pudo convertir el ORR persistido")
        return saved

    def get(self, orr_id: int) -> Optional[ModelORR]:
        entity = self._session.get(ORR, orr_id)
        return Entity2Model.orr(entity)

    def delete(self, orr_id: int) -> bool:
        entity = self._session.get(ORR, orr_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelORR) -> ModelORR:
        self._require_existing(ORR, model.id)
        return self.save(model)


class UserRepoSA(_RepoBase):
    def save(self, model: ModelUser) -> ModelUser:
        entity = Model2Entity.user(model)
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        return Entity2Model.user(merged)

    def get(self, user_id: int) -> Optional[ModelUser]:
        entity = self._session.get(User, user_id)
        return Entity2Model.user(entity) if entity else None

    def get_by_username(self, username: str) -> Optional[ModelUser]:
        entity = self._session.query(User).filter(User.username == username).one_or_none()
        return Entity2Model.user(entity) if entity else None

    def get_by_email(self, email: str) -> Optional[ModelUser]:
        entity = self._session.query(User).filter(User.email == email).one_or_none()
        return Entity2Model.user(entity) if entity else None

    def delete(self, user_id: int) -> bool:
        entity = self._session.get(User, user_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelUser) -> ModelUser:
        self._require_existing(User, model.id)
        return self.save(model)


class SAERepoSA(_RepoBase):
    def save(self, model: ModelSAE) -> ModelSAE:
        entity = Model2Entity.sae(model)
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        saved = Entity2Model.sae(merged)
        if saved is None:
            raise ValueError("No se pudo convertir el SAE persistido")
        return saved

    def get(self, sae_row_id: int) -> Optional[ModelSAE]:
        entity = self._session.get(SAE, sae_row_id)
        return Entity2Model.sae(entity)

    def get_by_sae_id(
        self,
        sae_id: str,
        simulation_id: Optional[int] = None,
    ) -> Optional[ModelSAE]:
        query = self._session.query(SAE).filter(SAE.sae_id == sae_id)
        if simulation_id is not None:
            query = query.filter(SAE.simulation_id == int(simulation_id))
        entity = query.one_or_none()
        return Entity2Model.sae(entity)

    def list_by_simulation(self, simulation_id: int) -> list[ModelSAE]:
        entities = (
            self._session.query(SAE)
            .filter(SAE.simulation_id == simulation_id)
            .order_by(SAE.id.asc())
            .all()
        )
        return [mapped for mapped in (Entity2Model.sae(entity) for entity in entities) if mapped]

    def list_by_dkms(self, dkms_id: int) -> list[ModelSAE]:
        entities = (
            self._session.query(SAE)
            .filter(SAE.dkms_id == dkms_id)
            .order_by(SAE.id.asc())
            .all()
        )
        return [mapped for mapped in (Entity2Model.sae(entity) for entity in entities) if mapped]

    def find_active_by_fingerprint(self, fingerprint: str) -> Optional[ModelSAE]:
        entity = (
            self._session.query(SAE)
            .filter(
                SAE.cert_fingerprint == fingerprint,
                SAE.status == SaeStatusEnum.ACTIVE,
            )
            .one_or_none()
        )
        return Entity2Model.sae(entity)

    def delete(self, sae_row_id: int) -> bool:
        entity = self._session.get(SAE, sae_row_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelSAE) -> ModelSAE:
        self._require_existing(SAE, model.id)
        return self.save(model)


class AgentControllerRepoSA(_RepoBase):
    def save(self, model: ModelAgentController) -> ModelAgentController:
        model = self._ensure_host(model, allow_none=True)
        entity = Model2Entity.agent_controller(model)
        merged = self._session.merge(entity)
        self._session.flush()
        self._session.refresh(merged)
        return Entity2Model.agent_controller(merged)

    def get(self, controller_id: int) -> Optional[ModelAgentController]:
        entity = self._session.get(AgentController, controller_id)
        return Entity2Model.agent_controller(entity) if entity else None

    def delete(self, controller_id: int) -> bool:
        entity = self._session.get(AgentController, controller_id)
        if not entity:
            return False
        self._session.delete(entity)
        return True

    def update(self, model: ModelAgentController) -> ModelAgentController:
        self._require_existing(AgentController, model.id)
        return self.save(model)

    def by_dkms_id(self, dkms_id: int) -> list[ModelAgentController]:
        dkms_entity = self._session.get(DKMS, dkms_id)
        if not dkms_entity:
            return []
        return [Entity2Model.agent_controller(ctrl) for ctrl in dkms_entity.agent_controllers]

    def by_orr_id(self, orr_id: int) -> list[ModelAgentController]:
        dkms_entities = self._session.query(DKMS).filter(DKMS.orr_id == orr_id).all()
        all_controllers: list[ModelAgentController] = []
        for dkms_entity in dkms_entities:
            for ctrl in dkms_entity.agent_controllers:
                all_controllers.append(Entity2Model.agent_controller(ctrl))
        return all_controllers

    def by_qkc_id(self, qkc_id: int) -> list[ModelAgentController]:
        orr_entities = self._session.query(ORR).filter(ORR.qkc_id == qkc_id).all()
        all_controllers: list[ModelAgentController] = []
        for orr_entity in orr_entities:
            dkms_entities = self._session.query(DKMS).filter(DKMS.orr_id == orr_entity.id).all()
            for dkms_entity in dkms_entities:
                for ctrl in dkms_entity.agent_controllers:
                    all_controllers.append(Entity2Model.agent_controller(ctrl))
        return all_controllers
