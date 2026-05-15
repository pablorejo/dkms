from __future__ import annotations

from typing import Optional

from models import (
    ModelAgentController,
    ModelDKMS,
    ModelHost,
    ModelORR,
    ModelQKC,
    ModelSAE,
    ModelSimulation,
    ModelUser,
)


class _MemoryRepoBase:
    """Repositorio base para operaciones comunes en memoria."""

    def __init__(self, store: dict, counter_key: str, collection_key: str) -> None:
        self._store = store
        self._counter_key = counter_key
        self._collection_key = collection_key

    def _next_id(self) -> int:
        counters = self._store["counters"]
        counters[self._counter_key] = counters.get(self._counter_key, 0) + 1
        return counters[self._counter_key]

    def _collection(self) -> dict:
        return self._store[self._collection_key]

    def _clone(self, model):
        return model.model_copy(deep=True)

    def _next_host_id(self) -> int:
        counters = self._store["counters"]
        counters["host"] = counters.get("host", 0) + 1
        return counters["host"]

    def _find_host(self, host: ModelHost) -> Optional[ModelHost]:
        for existing in self._store.get("hosts", {}).values():
            if (
                existing.id_simulation == host.id_simulation
                and existing.ip == host.ip
                and existing.port == host.port
            ):
                return existing
        return None

    def _ensure_host(self, model, allow_none: bool = False):
        hosts = self._store.setdefault("hosts", {})
        host = getattr(model, "host", None)
        id_host = getattr(model, "id_host", None)
        if host is None:
            if id_host is None:
                if allow_none:
                    return model
                raise ValueError("El id_host es obligatorio si no se provee host")
            if id_host not in hosts:
                raise ValueError(f"Host con id {id_host} no existe")
            return model

        host_id = host.id if host.id is not None else id_host
        existing = hosts.get(host_id) if host_id is not None else None
        if existing is None:
            existing = self._find_host(host)
        if existing is not None:
            host_id = existing.id
        else:
            if host_id is None:
                host_id = self._next_host_id()
            else:
                counters = self._store["counters"]
                counters["host"] = max(counters.get("host", 0), host_id)
            host = host.model_copy(update={"id": host_id})
            hosts[host_id] = self._clone(host)
        host = host.model_copy(update={"id": host_id})
        return model.model_copy(update={"id_host": host_id, "host": host})


class SimulationRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "sim", "sims")

    def save(self, model: ModelSimulation) -> ModelSimulation:
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, simulation_id: int) -> Optional[ModelSimulation]:
        model = self._collection().get(simulation_id)
        return self._clone(model) if model else None

    def delete(self, simulation_id: int) -> bool:
        if simulation_id not in self._collection():
            return False
        del self._collection()[simulation_id]
        return True

    def update(self, model: ModelSimulation) -> ModelSimulation:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar Simulation")
        if model.id not in self._collection():
            raise ValueError(f"Simulation con id {model.id} no existe")
        return self.save(model)


class DKMSRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "dkms", "dkms")

    def save(self, model: ModelDKMS) -> ModelDKMS:
        model = self._ensure_host(model)
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, dkms_id: int) -> Optional[ModelDKMS]:
        model = self._collection().get(dkms_id)
        return self._clone(model) if model else None

    def delete(self, dkms_id: int) -> bool:
        if dkms_id not in self._collection():
            return False
        del self._collection()[dkms_id]
        return True

    def update(self, model: ModelDKMS) -> ModelDKMS:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar DKMS")
        if model.id not in self._collection():
            raise ValueError(f"DKMS con id {model.id} no existe")
        return self.save(model)


class QKCRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "qkc", "qkcs")

    def save(self, model: ModelQKC) -> ModelQKC:
        model = self._ensure_host(model)
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, qkc_id: int) -> Optional[ModelQKC]:
        model = self._collection().get(qkc_id)
        return self._clone(model) if model else None

    def delete(self, qkc_id: int) -> bool:
        if qkc_id not in self._collection():
            return False
        del self._collection()[qkc_id]
        return True

    def update(self, model: ModelQKC) -> ModelQKC:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar QKC")
        if model.id not in self._collection():
            raise ValueError(f"QKC con id {model.id} no existe")
        return self.save(model)


class ORRRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "orr", "orrs")

    def save(self, model: ModelORR) -> ModelORR:
        model = self._ensure_host(model)
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, orr_id: int) -> Optional[ModelORR]:
        model = self._collection().get(orr_id)
        return self._clone(model) if model else None

    def delete(self, orr_id: int) -> bool:
        if orr_id not in self._collection():
            return False
        del self._collection()[orr_id]
        return True

    def update(self, model: ModelORR) -> ModelORR:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar ORR")
        if model.id not in self._collection():
            raise ValueError(f"ORR con id {model.id} no existe")
        return self.save(model)


class UserRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "user", "users")

    def save(self, model: ModelUser) -> ModelUser:
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, user_id: int) -> Optional[ModelUser]:
        model = self._collection().get(user_id)
        return self._clone(model) if model else None

    def get_by_username(self, username: str) -> Optional[ModelUser]:
        for model in self._collection().values():
            if model.username == username:
                return self._clone(model)
        return None

    def get_by_email(self, email: str) -> Optional[ModelUser]:
        for model in self._collection().values():
            if model.email == email:
                return self._clone(model)
        return None

    def delete(self, user_id: int) -> bool:
        if user_id not in self._collection():
            return False
        del self._collection()[user_id]
        return True

    def update(self, model: ModelUser) -> ModelUser:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar User")
        if model.id not in self._collection():
            raise ValueError(f"User con id {model.id} no existe")
        return self.save(model)


class SAERepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "sae", "saes")

    def save(self, model: ModelSAE) -> ModelSAE:
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, sae_row_id: int) -> Optional[ModelSAE]:
        model = self._collection().get(sae_row_id)
        return self._clone(model) if model else None

    def get_by_sae_id(
        self,
        sae_id: str,
        simulation_id: Optional[int] = None,
    ) -> Optional[ModelSAE]:
        for model in self._collection().values():
            if str(model.sae_id or "") != str(sae_id):
                continue
            if simulation_id is not None and int(getattr(model, "simulation_id", 0) or 0) != int(simulation_id):
                continue
            return self._clone(model)
        return None

    def list_by_simulation(self, simulation_id: int) -> list[ModelSAE]:
        return [
            self._clone(model)
            for model in sorted(self._collection().values(), key=lambda item: int(item.id or 0))
            if model.simulation_id == simulation_id
        ]

    def list_by_dkms(self, dkms_id: int) -> list[ModelSAE]:
        return [
            self._clone(model)
            for model in sorted(self._collection().values(), key=lambda item: int(item.id or 0))
            if model.dkms_id == dkms_id
        ]

    def find_active_by_fingerprint(self, fingerprint: str) -> Optional[ModelSAE]:
        fingerprint_value = str(fingerprint or "")
        for model in self._collection().values():
            if (
                str(model.cert_fingerprint or "") == fingerprint_value
                and str(getattr(model.status, "value", model.status)) == "active"
            ):
                return self._clone(model)
        return None

    def delete(self, sae_row_id: int) -> bool:
        if sae_row_id not in self._collection():
            return False
        del self._collection()[sae_row_id]
        return True

    def update(self, model: ModelSAE) -> ModelSAE:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar SAE")
        if model.id not in self._collection():
            raise ValueError(f"SAE con id {model.id} no existe")
        return self.save(model)


class AgentControllerRepoMemory(_MemoryRepoBase):
    def __init__(self, store: dict) -> None:
        super().__init__(store, "agent_controller", "agent_controllers")

    def _controllers(self) -> list[ModelAgentController]:
        return list(self._store.get("agent_controllers", {}).values())

    def save(self, model: ModelAgentController) -> ModelAgentController:
        model = self._ensure_host(model, allow_none=True)
        if model.id is None:
            model = model.model_copy(update={"id": self._next_id()})
        self._collection()[model.id] = self._clone(model)
        return self._clone(model)

    def get(self, controller_id: int) -> Optional[ModelAgentController]:
        model = self._collection().get(controller_id)
        return self._clone(model) if model else None

    def delete(self, controller_id: int) -> bool:
        if controller_id not in self._collection():
            return False
        del self._collection()[controller_id]
        return True

    def update(self, model: ModelAgentController) -> ModelAgentController:
        if model.id is None:
            raise ValueError("El identificador es obligatorio para actualizar AgentController")
        if model.id not in self._collection():
            raise ValueError(f"AgentController con id {model.id} no existe")
        return self.save(model)

    def by_dkms_id(self, dkms_id: int) -> list[ModelAgentController]:
        return [
            controller.model_copy(deep=True)
            for controller in self._controllers()
            if controller.id_dkms == dkms_id
        ]

    def by_orr_id(self, orr_id: int) -> list[ModelAgentController]:
        dkms_ids = [
            dkms.id
            for dkms in self._store.get("dkms", {}).values()
            if dkms.id is not None and dkms.orr_id == orr_id
        ]
        if not dkms_ids:
            return []
        return [
            controller.model_copy(deep=True)
            for controller in self._controllers()
            if controller.id_dkms in dkms_ids
        ]

    def by_qkc_id(self, qkc_id: int) -> list[ModelAgentController]:
        orr_ids = [
            orr.id
            for orr in self._store.get("orrs", {}).values()
            if orr.id is not None and orr.qkc_id == qkc_id
        ]
        if not orr_ids:
            return []
        dkms_ids = [
            dkms.id
            for dkms in self._store.get("dkms", {}).values()
            if dkms.id is not None and dkms.orr_id in orr_ids
        ]
        if not dkms_ids:
            return []
        return [
            controller.model_copy(deep=True)
            for controller in self._controllers()
            if controller.id_dkms in dkms_ids
        ]
