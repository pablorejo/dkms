from __future__ import annotations

from typing import Iterable, Optional

from models import (
    Channel,
    ChannelType,
    CipherDKMS,
    ETSIType,
    KMEConfig,
    ModelAgentController,
    ModelDKMS,
    ModelFile,
    ModelHost,
    ModelORR,
    ModelQKC,
    ModelSAE,
    ModelSDN,
    ModelSimulation,
    ModelUser,
    SaeStatus,
    SimulationStatus,
    TLSConfigDKMS,
    TLSConfigSAE,
    TLSVersion,
    TokenBucketConfig,
)
from models.enums import HTTPType

from .data import (
    AgentController,
    DataFile,
    DKMS,
    Host,
    KME,
    ORR,
    QKC,
    SAE,
    SDN,
    Simulation,
    TLSCipher,
    TLSConfigDKMS as TLSConfigDKMSEntity,
    TLSConfigSAE as TLSConfigSAEEntity,
    TokenBucket,
    User,
)


def _enum_value(value):
    """Devuelve el valor primitivo de un Enum SQLAlchemy/Pydantic."""
    if value is None:
        return None
    return getattr(value, "value", value)


class Model2Entity:
    """Conversion de modelos Pydantic a entidades SQLAlchemy."""

    @staticmethod
    def data_file(model: Optional[ModelFile]) -> Optional[DataFile]:
        if model is None:
            return None
        return DataFile(id=model.id, path=model.path, data=model.data)

    @staticmethod
    def tls_dkms(model: TLSConfigDKMS) -> TLSConfigDKMSEntity:
        entity = TLSConfigDKMSEntity(
            id=model.id,
            require_client_cert=model.require_client_cert,
            version=_enum_value(model.version),
        )
        entity.cert = Model2Entity.data_file(model.cert)
        entity.key = Model2Entity.data_file(model.key)
        entity.ca_cert = Model2Entity.data_file(model.ca_cert)
        unique_ciphers: Iterable[CipherDKMS] = list(dict.fromkeys(model.ciphers))
        entity.ciphers = [
            TLSCipher(cipher=_enum_value(cipher)) for cipher in unique_ciphers
        ]
        return entity

    @staticmethod
    def tls_sae(model: TLSConfigSAE) -> TLSConfigSAEEntity:
        entity = TLSConfigSAEEntity(
            id=model.id,
            use_client_cert=model.use_client_cert,
        )
        entity.cert = Model2Entity.data_file(model.cert)
        entity.key = Model2Entity.data_file(model.key)
        entity.ca_certs = Model2Entity.data_file(model.ca_certs)
        return entity

    @staticmethod
    def token_bucket(model: TokenBucketConfig) -> TokenBucket:
        return TokenBucket(
            beta=model.beta,
            gamma=model.gamma,
            delta_up=model.delta_up,
            delta_down=model.delta_down,
            T_obs=model.T_obs,
            alpha0=model.alpha0,
            alpha_min=model.alpha_min,
            tau=model.tau,
            B_min=model.B_min,
            B_max=model.B_max,
            R_max=model.R_max,
            initial_tokens=model.initial_tokens,
            status_timeout=model.status_timeout,
            verify=model.verify,
        )

    @staticmethod
    def kme(model: KMEConfig, *, id_simulation: int) -> KME:
        """Crea entidad KME ligada a una sim concreta vía id_simulation.

        2026-05-23: id_simulation se vuelve **obligatorio** porque qkc.id es
        un ID lógico (derivado de node_id_offset) reusado entre sims; sin
        id_simulation el `QKC.local_kmes` relationship leakea KMEs entre
        sims (ver project_bd_orphan_kmes_inflate_sdn).
        """
        url_node_qkd = getattr(model, "local_url_node_qkd", None)
        if url_node_qkd is None:
            url_node_qkd = getattr(model, "url_node_qkd", None)
        neighbor_qkd = getattr(model, "neighbor_qkd_id", None)
        if neighbor_qkd is None:
            neighbor_qkd = getattr(model, "neighbor_qkd", None)
        channel = getattr(model, "channel", None)
        channel_type = getattr(channel, "type_channel", None)
        if channel_type is None:
            channel_type = (
                ChannelType.PQC_SIMULATION
                if bool(getattr(model, "pqc_simulation", False))
                else ChannelType.QKD
            )
        channel_distance = getattr(channel, "distance", 0) if channel is not None else 0
        channel_quditto_max_buffer_size = (
            getattr(channel, "quditto_max_buffer_size", 10000) if channel is not None else 10000
        )
        channel_quditto_rate_r0 = (
            getattr(channel, "quditto_rate_r0", 2000.0) if channel is not None else 2000.0
        )
        channel_quditto_rate_alpha = (
            getattr(channel, "quditto_rate_alpha", 0.2) if channel is not None else 0.2
        )
        entity = KME(
            id=model.id,
            id_simulation=int(id_simulation),
            local_qkc_id=model.local_qkc_id,
            neighbor_qkc_id=model.neighbor_qkc_id,
            url_node_QKD=url_node_qkd,
            neighbor_QKD=neighbor_qkd,
            etsi=_enum_value(model.etsi),
            channel_type=_enum_value(channel_type),
            channel_distance=int(channel_distance or 0),
            channel_quditto_max_buffer_size=max(1, int(channel_quditto_max_buffer_size or 10000)),
            channel_quditto_rate_r0=float(channel_quditto_rate_r0) if float(channel_quditto_rate_r0) > 0 else 2000.0,
            channel_quditto_rate_alpha=max(0.0, float(channel_quditto_rate_alpha)),
            pqc_simulation=bool(getattr(model, "pqc_simulation", False)),
            hybrid_enabled=bool(getattr(model, "hybrid_enabled", False)),
            pqc_kme_port=int(getattr(model, "pqc_kme_port", 6000) or 6000),
        )
        entity.cert = Model2Entity.data_file(model.cert)
        entity.key = Model2Entity.data_file(model.key)
        if model.token_bucket:
            entity.token_bucket = Model2Entity.token_bucket(model.token_bucket)
        return entity

    @staticmethod
    def qkc(model: ModelQKC, *, id_simulation: int) -> QKC:
        """Crea entidad QKC ligada a una sim concreta.

        2026-05-23: id_simulation obligatorio; propaga a sus KMEs.
        """
        entity = QKC(
            id=model.id,
            id_host=model.id_host,
            id_simulation=int(id_simulation),
            kme_host=model.kme_host,
        )
        if model.host:
            entity.host = Model2Entity.host(model.host)
            if model.host.id is not None:
                entity.id_host = model.host.id
        entity.local_kmes = [
            Model2Entity.kme(kme_cfg, id_simulation=id_simulation)
            for kme_cfg in model.kmes
        ]
        return entity

    @staticmethod
    def orr(model: ModelORR, *, id_simulation: int) -> ORR:
        """Crea entidad ORR + propaga id_simulation a su QKC."""
        entity = ORR(
            id=model.id,
            id_host=model.id_host,
            qkc_id=model.qkc_id,
        )
        if model.host:
            entity.host = Model2Entity.host(model.host)
            if model.host.id is not None:
                entity.id_host = model.host.id
        if model.qkc:
            entity.qkc = Model2Entity.qkc(model.qkc, id_simulation=id_simulation)
            if model.qkc.id is not None:
                entity.qkc_id = model.qkc.id
        return entity

    @staticmethod
    def host(model: ModelHost) -> Host:
        entity = Host(
            id=model.id,
            id_simulation=model.id_simulation,
            ip=model.ip,
            port=model.port,
        )
        if model.simulation:
            entity.simulation = Model2Entity.simulation(model.simulation)
        return entity

    @staticmethod
    def simulation(model: ModelSimulation) -> Simulation:
        entity = Simulation(
            id=model.id,
            id_user=model.id_user,
            name=model.name,
            description=model.description,
            editor_topology_json=model.editor_topology_json,
            created_at=model.created_at,
            updated_at=model.updated_at,
            start_time=model.start_time,
            end_time=model.end_time,
            status=_enum_value(model.status),
        )
        if model.user:
            entity.user = Model2Entity.user(model.user)
        return entity

    @staticmethod
    def user(model: ModelUser) -> User:
        return User(
            id=model.id,
            username=model.username,
            email=model.email,
            password_hash=model.password_hash,
            is_active=model.is_active,
        )

    @staticmethod
    def sdn(model: ModelSDN) -> SDN:
        entity = SDN(
            id=model.id,
            id_host=model.id_host,
            type_http=_enum_value(model.type_http),
        )
        if model.host:
            entity.host = Model2Entity.host(model.host)
            if model.host.id is not None:
                entity.id_host = model.host.id
        return entity

    @staticmethod
    def agent_controller(model: ModelAgentController) -> AgentController:
        entity = AgentController(
            id=model.id,
            id_dkms=model.id_dkms,
            id_sdn=model.id_sdn,
            id_host=model.id_host,
        )
        if model.host:
            entity.host = Model2Entity.host(model.host)
            if model.host.id is not None:
                entity.id_host = model.host.id
        if model.sdn:
            entity.sdn = Model2Entity.sdn(model.sdn)
            if model.sdn.id is not None:
                entity.id_sdn = model.sdn.id
        return entity

    @staticmethod
    def dkms(model: ModelDKMS, *, id_simulation: int) -> DKMS:
        """Crea entidad DKMS + propaga id_simulation a su ORR/QKC."""
        entity = DKMS(
            id=model.id,
            id_host=model.id_host,
            orr_id=model.orr_id,
            tls_id=model.tls_id,
        )
        if model.host:
            entity.host = Model2Entity.host(model.host)
            if model.host.id is not None:
                entity.id_host = model.host.id
        if model.orr:
            entity.orr = Model2Entity.orr(model.orr, id_simulation=id_simulation)
            if model.orr.id is not None:
                entity.orr_id = model.orr.id
        if model.tls:
            entity.tls_config = Model2Entity.tls_dkms(model.tls)
            if model.tls.id is not None:
                entity.tls_id = model.tls.id
        # entity.agent_controllers = [
        #     Model2Entity.agent_controller(ctrl) for ctrl in model.agent_controllers
        # ]
        return entity

    @staticmethod
    def sae(model: ModelSAE) -> SAE:
        entity = SAE(
            id=model.id,
            sdn_id=model.sdn_id,
            tls_id=model.tls_id,
            agent_dkms_id=model.agent_dkms_id,
            sae_id=model.sae_id,
            display_name=model.display_name,
            owner_user_id=model.owner_user_id,
            simulation_id=model.simulation_id,
            dkms_id=model.dkms_id,
            status=_enum_value(model.status or SaeStatus.PENDING_CERT),
            cert_serial=model.cert_serial,
            cert_fingerprint=model.cert_fingerprint,
            cert_subject=model.cert_subject,
            cert_not_before=model.cert_not_before,
            cert_not_after=model.cert_not_after,
            revoked_at=model.revoked_at,
            revocation_reason=model.revocation_reason,
            created_at=model.created_at,
            updated_at=model.updated_at,
        )
        if model.sdn:
            entity.sdn = Model2Entity.sdn(model.sdn)
            if model.sdn.id is not None:
                entity.sdn_id = model.sdn.id
        if model.tls:
            entity.tls_config = Model2Entity.tls_sae(model.tls)
            if model.tls.id is not None:
                entity.tls_id = model.tls.id
        if model.agent_controller:
            entity.agent_controller = Model2Entity.agent_controller(model.agent_controller)
            if model.agent_controller.id is not None:
                entity.agent_dkms_id = model.agent_controller.id
        return entity


class Entity2Model:
    """Conversion de entidades SQLAlchemy a modelos Pydantic."""

    @staticmethod
    def data_file(entity: Optional[DataFile]) -> ModelFile:
        if entity is None:
            raise ValueError("Se esperaba un DataFile asociado al registro")
        return ModelFile(id=entity.id, path=entity.path, data=entity.data)

    @staticmethod
    def tls_dkms(entity: Optional[TLSConfigDKMSEntity]) -> Optional[TLSConfigDKMS]:
        if entity is None:
            return None
        version_value = _enum_value(entity.version)
        ciphers = sorted(entity.ciphers, key=lambda cipher: cipher.cipher)
        cipher_values = [_enum_value(cipher.cipher) for cipher in ciphers]
        return TLSConfigDKMS(
            id=entity.id,
            cert=Entity2Model.data_file(entity.cert),
            key=Entity2Model.data_file(entity.key),
            ca_cert=Entity2Model.data_file(entity.ca_cert),
            require_client_cert=entity.require_client_cert,
            version=TLSVersion(version_value),
            ciphers=[CipherDKMS(value) for value in cipher_values],
        )

    @staticmethod
    def tls_sae(entity: Optional[TLSConfigSAEEntity]) -> Optional[TLSConfigSAE]:
        if entity is None:
            return None
        return TLSConfigSAE(
            id=entity.id,
            cert=Entity2Model.data_file(entity.cert),
            key=Entity2Model.data_file(entity.key),
            ca_certs=Entity2Model.data_file(entity.ca_certs),
            use_client_cert=entity.use_client_cert,
        )

    @staticmethod
    def token_bucket(entity: Optional[TokenBucket]) -> Optional[TokenBucketConfig]:
        if entity is None:
            return None
        return TokenBucketConfig(
            beta=entity.beta,
            gamma=entity.gamma,
            delta_up=entity.delta_up,
            delta_down=entity.delta_down,
            T_obs=entity.T_obs,
            alpha0=entity.alpha0,
            alpha_min=entity.alpha_min,
            tau=entity.tau,
            B_min=entity.B_min,
            B_max=entity.B_max,
            R_max=entity.R_max,
            initial_tokens=entity.initial_tokens,
            status_timeout=entity.status_timeout,
            verify=entity.verify,
        )

    @staticmethod
    def kme(entity: KME) -> KMEConfig:
        etsi_value = _enum_value(entity.etsi)
        local_qkc_host = entity.local_qkc.host if entity.local_qkc else None
        neighbor_qkc_host = entity.neighbor_qkc.host if entity.neighbor_qkc else None
        local_qkc_ip = local_qkc_host.ip if local_qkc_host else ""
        neighbor_qkc_ip = neighbor_qkc_host.ip if neighbor_qkc_host else ""
        neighbor_qkc_port = neighbor_qkc_host.port if neighbor_qkc_host else 0
        local_qkd_id = str(entity.local_qkc_id) if entity.local_qkc_id is not None else ""
        raw_channel_type = _enum_value(getattr(entity, "channel_type", None))
        if raw_channel_type is None:
            raw_channel_type = (
                ChannelType.PQC_SIMULATION.value
                if bool(getattr(entity, "pqc_simulation", False))
                else ChannelType.QKD.value
            )
        try:
            channel_type = ChannelType(raw_channel_type)
        except ValueError:
            channel_type = ChannelType.QKD
        channel_distance = int(getattr(entity, "channel_distance", 0) or 0)
        channel_quditto_max_buffer_size = max(
            1, int(getattr(entity, "channel_quditto_max_buffer_size", 10000) or 10000)
        )
        channel_quditto_rate_r0 = float(getattr(entity, "channel_quditto_rate_r0", 2000.0))
        channel_quditto_rate_alpha = max(
            0.0, float(getattr(entity, "channel_quditto_rate_alpha", 0.2))
        )
        return KMEConfig(
            id=entity.id,
            local_qkc_id=entity.local_qkc_id,
            local_qkc_ip=local_qkc_ip,
            neighbor_qkc_id=entity.neighbor_qkc_id,
            neighbor_qkc_ip=neighbor_qkc_ip,
            neighbor_qkc_port=neighbor_qkc_port,
            neighbor_qkd_id=entity.neighbor_QKD,
            local_qkd_id=local_qkd_id,
            local_url_node_qkd=entity.url_node_QKD,
            etsi=ETSIType(etsi_value),
            cert=Entity2Model.data_file(entity.cert),
            key=Entity2Model.data_file(entity.key),
            token_bucket=Entity2Model.token_bucket(entity.token_bucket),
            channel=Channel(
                type_channel=channel_type,
                distance=channel_distance,
                quditto_max_buffer_size=channel_quditto_max_buffer_size,
                quditto_rate_r0=channel_quditto_rate_r0,
                quditto_rate_alpha=channel_quditto_rate_alpha,
            ),
            pqc_simulation=bool(getattr(entity, "pqc_simulation", False)),
            hybrid_enabled=bool(getattr(entity, "hybrid_enabled", False)),
            pqc_kme_port=int(getattr(entity, "pqc_kme_port", 6000) or 6000),
        )

    @staticmethod
    def host(entity: Host, include_simulation: bool = True) -> ModelHost:
        return ModelHost(
            id=entity.id,
            id_simulation=entity.id_simulation,
            ip=entity.ip,
            port=entity.port,
            simulation=Entity2Model.simulation(entity.simulation)
            if include_simulation and entity.simulation
            else None,
        )

    @staticmethod
    def simulation(entity: Simulation) -> ModelSimulation:
        status_value = _enum_value(entity.status)
        dkms_models: list[ModelDKMS] = []
        sdn_model: Optional[ModelSDN] = None
        for host in entity.hosts or []:
            for dkms_entity in host.dkms_nodes or []:
                dkms_model = Entity2Model.dkms(dkms_entity)
                if dkms_model is not None:
                    dkms_models.append(dkms_model)
            if sdn_model is None and host.sdn_nodes:
                sdn_model = Entity2Model.sdn(host.sdn_nodes[0])
        if sdn_model is None:
            sdn_model = ModelSDN(
                id=None,
                id_host=None,
                host=None,
                type_http=HTTPType.HTTP,
            )
        return ModelSimulation(
            id=entity.id,
            id_user=entity.id_user,
            name=entity.name,
            description=entity.description,
            editor_topology_json=entity.editor_topology_json,
            created_at=entity.created_at,
            updated_at=entity.updated_at,
            start_time=entity.start_time,
            end_time=entity.end_time,
            status=SimulationStatus(status_value) if status_value else None,
            user=Entity2Model.user(entity.user) if entity.user else None,
            list_dkms=dkms_models,
            sdn=sdn_model,
        )

    @staticmethod
    def user(entity: User) -> ModelUser:
        return ModelUser(
            id=entity.id,
            username=entity.username,
            email=entity.email,
            password_hash=entity.password_hash,
            is_active=entity.is_active,
        )

    @staticmethod
    def sdn(entity: SDN) -> ModelSDN:
        raw_type = _enum_value(getattr(entity, "type_http", None)) or "http"
        return ModelSDN(
            id=entity.id,
            id_host=entity.id_host,
            host=Entity2Model.host(entity.host, include_simulation=False) if entity.host else None,
            type_http=HTTPType(raw_type),
        )

    @staticmethod
    def agent_controller(entity: AgentController) -> ModelAgentController:
        return ModelAgentController(
            id=entity.id,
            id_dkms=entity.id_dkms,
            id_sdn=entity.id_sdn,
            id_host=entity.id_host,
            sdn=Entity2Model.sdn(entity.sdn) if entity.sdn else None,
            host=Entity2Model.host(entity.host, include_simulation=False) if entity.host else None,
        )

    @staticmethod
    def qkc(entity: Optional[QKC]) -> Optional[ModelQKC]:
        if entity is None:
            return None
        return ModelQKC(
            id=entity.id,
            id_host=entity.id_host,
            kme_host=entity.kme_host,
            host=Entity2Model.host(entity.host, include_simulation=False) if entity.host else None,
            kmes=[Entity2Model.kme(kme) for kme in entity.local_kmes],
        )

    @staticmethod
    def orr(entity: Optional[ORR]) -> Optional[ModelORR]:
        if entity is None:
            return None
        return ModelORR(
            id=entity.id,
            id_host=entity.id_host,
            qkc_id=entity.qkc_id,
            host=Entity2Model.host(entity.host, include_simulation=False) if entity.host else None,
            qkc=Entity2Model.qkc(entity.qkc),
        )

    @staticmethod
    def dkms(entity: Optional[DKMS]) -> Optional[ModelDKMS]:
        if entity is None:
            return None
        return ModelDKMS(
            id=entity.id,
            id_host=entity.id_host,
            orr_id=entity.orr_id,
            tls_id=entity.tls_id,
            host=Entity2Model.host(entity.host, include_simulation=False) if entity.host else None,
            orr=Entity2Model.orr(entity.orr),
            tls=Entity2Model.tls_dkms(entity.tls_config),
            # agent_controllers=[
            #     Entity2Model.agent_controller(ctrl) for ctrl in entity.agent_controllers
            # ],
        )

    @staticmethod
    def sae(entity: Optional[SAE]) -> Optional[ModelSAE]:
        if entity is None:
            return None
        raw_status = _enum_value(entity.status)
        if raw_status is None:
            raw_status = SaeStatus.PENDING_CERT.value
        return ModelSAE(
            id=entity.id,
            sae_id=entity.sae_id,
            display_name=entity.display_name,
            sdn_id=entity.sdn_id,
            tls_id=entity.tls_id,
            agent_dkms_id=entity.agent_dkms_id,
            owner_user_id=entity.owner_user_id,
            simulation_id=entity.simulation_id,
            dkms_id=entity.dkms_id,
            status=SaeStatus(raw_status),
            cert_serial=entity.cert_serial,
            cert_fingerprint=entity.cert_fingerprint,
            cert_subject=entity.cert_subject,
            cert_not_before=entity.cert_not_before,
            cert_not_after=entity.cert_not_after,
            revoked_at=entity.revoked_at,
            revocation_reason=entity.revocation_reason,
            created_at=entity.created_at,
            updated_at=entity.updated_at,
            sdn=Entity2Model.sdn(entity.sdn) if entity.sdn else None,
            tls=Entity2Model.tls_sae(entity.tls_config),
            agent_controller=Entity2Model.agent_controller(entity.agent_controller)
            if entity.agent_controller
            else None,
            dkms_target=None,
        )
