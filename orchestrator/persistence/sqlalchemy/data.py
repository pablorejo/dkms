from __future__ import annotations

from enum import Enum

from sqlalchemy import (
    Boolean,
    CheckConstraint,
    Column,
    DateTime,
    Enum as SAEnum,
    Float,
    ForeignKey,
    Integer,
    String,
    Text,
    UniqueConstraint,
    func,
)
from sqlalchemy.orm import declarative_base, relationship

Base = declarative_base()


class ETSIEnum(str, Enum):
    ETSI_014 = "ETSI_014"
    ETSI_004 = "ETSI_004"


class ChannelTypeEnum(str, Enum):
    PQC_SIMULATION = "pqc-simulation"
    QKD = "qkd"


class CipherDKMSEnum(str, Enum):
    TLS_AES_256_GCM_SHA384 = "TLS_AES_256_GCM_SHA384"
    TLS_CHACHA20_POLY1305_SHA256 = "TLS_CHACHA20_POLY1305_SHA256"
    TLS_AES_128_GCM_SHA256 = "TLS_AES_128_GCM_SHA256"


class TLSVersionEnum(str, Enum):
    TLSV1_3 = "TLSv1_3"
    TLSV1_2 = "TLSv1_2"


class SimulationStatusEnum(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    FINISHED = "finished"
    ERROR = "error"


class HTTPTypeEnum(str, Enum):
    HTTP = "http"
    HTTPS = "https"


class SimulationRunStatusEnum(str, Enum):
    QUEUED = "QUEUED"
    RUNNING = "RUNNING"
    DONE = "DONE"
    FAILED = "FAILED"


class SaeStatusEnum(str, Enum):
    PENDING_CERT = "pending_cert"
    ACTIVE = "active"
    REVOKED = "revoked"
    EXPIRED = "expired"


def _enum_values(enum_cls) -> list[str]:
    return [member.value for member in enum_cls]


class DataFile(Base):
    __tablename__ = "data_file"

    id = Column(Integer, primary_key=True, autoincrement=True)
    path = Column(String(512), nullable=False)
    data = Column(Text, nullable=True)


class User(Base):
    __tablename__ = "user"

    id = Column(Integer, primary_key=True, autoincrement=True)
    username = Column(String(50), nullable=False, unique=True)
    email = Column(String(120), nullable=False, unique=True)
    password_hash = Column(String(128), nullable=False)
    is_active = Column(Boolean, nullable=False, default=False)

    simulations = relationship("Simulation", back_populates="user")


class Simulation(Base):
    __tablename__ = "simulation"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_user = Column(
        Integer,
        ForeignKey("user.id", onupdate="CASCADE", ondelete="CASCADE"),
        nullable=False,
    )
    name = Column(String(255), nullable=False)
    description = Column(Text, nullable=True)
    start_time = Column(DateTime(timezone=True), nullable=True)
    end_time = Column(DateTime(timezone=True), nullable=True)
    status = Column(
        SAEnum(SimulationStatusEnum, name="status", values_callable=_enum_values),
        nullable=True,
    )
    editor_topology_json = Column(Text, nullable=True)
    created_at = Column(DateTime(timezone=True), nullable=False, default=func.now())
    updated_at = Column(
        DateTime(timezone=True),
        nullable=False,
        default=func.now(),
        onupdate=func.now(),
    )

    user = relationship("User", back_populates="simulations")
    hosts = relationship("Host", back_populates="simulation", cascade="all, delete-orphan")
    runs = relationship("SimulationRun", back_populates="simulation", cascade="all, delete-orphan")


class Host(Base):
    __tablename__ = "host"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_simulation = Column(
        Integer,
        ForeignKey("simulation.id", onupdate="CASCADE", ondelete="CASCADE"),
        nullable=False,
    )
    ip = Column(String(15), nullable=False)
    port = Column(Integer, nullable=False)

    simulation = relationship("Simulation", back_populates="hosts")
    qkc_nodes = relationship("QKC", back_populates="host", cascade="all, delete-orphan")
    orr_nodes = relationship("ORR", back_populates="host", cascade="all, delete")
    dkms_nodes = relationship("DKMS", back_populates="host", cascade="all, delete")
    sdn_nodes = relationship("SDN", back_populates="host", cascade="all, delete-orphan")
    agent_controllers = relationship(
        "AgentController", back_populates="host", cascade="all, delete"
    )

    __table_args__ = (
        UniqueConstraint("id_simulation", "ip", "port", name="host_index_0"),
        CheckConstraint(
            "ip ~ '^((25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})\\.){3}(25[0-5]|2[0-4][0-9]|1?[0-9]{1,2})$'",
            name="ck_host_ipv4",
        ),
    )


class QKC(Base):
    __tablename__ = "qkc"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_host = Column(
        Integer,
        ForeignKey("host.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    kme_host = Column(String(255), nullable=True)

    host = relationship("Host", back_populates="qkc_nodes")
    orrs = relationship("ORR", back_populates="qkc", cascade="all, delete")
    local_kmes = relationship(
        "KME",
        back_populates="local_qkc",
        foreign_keys="KME.local_qkc_id",
        cascade="all, delete",
    )
    neighbor_kmes = relationship(
        "KME",
        back_populates="neighbor_qkc",
        foreign_keys="KME.neighbor_qkc_id",
        cascade="all, delete",
    )


class TLSConfigDKMS(Base):
    __tablename__ = "tls_config_dkms"

    id = Column(Integer, primary_key=True, autoincrement=True)
    cert_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    key_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    ca_cert_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    require_client_cert = Column(Boolean, nullable=False, default=False)
    version = Column(
        SAEnum(TLSVersionEnum, name="version_tls", values_callable=_enum_values),
        nullable=False,
    )

    cert = relationship("DataFile", foreign_keys=[cert_id])
    key = relationship("DataFile", foreign_keys=[key_id])
    ca_cert = relationship("DataFile", foreign_keys=[ca_cert_id])
    dkms_instances = relationship("DKMS", back_populates="tls_config")
    ciphers = relationship("TLSCipher", back_populates="tls_config", cascade="all, delete-orphan")


class TLSCipher(Base):
    __tablename__ = "tls_cipher"

    id = Column(Integer, primary_key=True, autoincrement=True)
    cipher = Column(
        SAEnum(CipherDKMSEnum, name="cipherdkms", values_callable=_enum_values),
        nullable=False,
    )
    tls_config_dkms_id = Column(
        Integer,
        ForeignKey("tls_config_dkms.id", onupdate="CASCADE", ondelete="CASCADE"),
        nullable=False,
    )

    tls_config = relationship("TLSConfigDKMS", back_populates="ciphers")

    __table_args__ = (
        UniqueConstraint("cipher", "tls_config_dkms_id", name="tls_cipher_index_0"),
    )


class TLSConfigSAE(Base):
    __tablename__ = "tls_config_sae"

    id = Column(Integer, primary_key=True, autoincrement=True)
    cert_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    key_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    ca_certs_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    use_client_cert = Column(Boolean, nullable=False, default=False)

    cert = relationship("DataFile", foreign_keys=[cert_id])
    key = relationship("DataFile", foreign_keys=[key_id])
    ca_certs = relationship("DataFile", foreign_keys=[ca_certs_id])
    saes = relationship("SAE", back_populates="tls_config")


class SDN(Base):
    __tablename__ = "sdn"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_host = Column(
        Integer,
        ForeignKey("host.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    type_http = Column(
        SAEnum(HTTPTypeEnum, name="type_http", values_callable=_enum_values),
        nullable=False,
        default=HTTPTypeEnum.HTTP,
    )

    host = relationship("Host", back_populates="sdn_nodes")
    agent_controllers = relationship(
        "AgentController", back_populates="sdn", cascade="all, delete"
    )
    # Mantener SAE como entidad persistente independiente del lifecycle del SDN.
    # En edición de topología se recrea SDN/hosts y no debe arrastrar borrado de SAE.
    saes = relationship("SAE", back_populates="sdn")


class ORR(Base):
    __tablename__ = "orr"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_host = Column(
        Integer,
        ForeignKey("host.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    qkc_id = Column(
        Integer,
        ForeignKey("qkc.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
        index=True,
    )

    host = relationship("Host", back_populates="orr_nodes")
    qkc = relationship("QKC", back_populates="orrs")
    dkms_instances = relationship("DKMS", back_populates="orr", cascade="all, delete")


class DKMS(Base):
    __tablename__ = "dkms"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_host = Column(
        Integer,
        ForeignKey("host.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    orr_id = Column(
        Integer,
        ForeignKey("orr.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
        index=True,
    )
    tls_id = Column(
        Integer,
        ForeignKey("tls_config_dkms.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
    )

    host = relationship("Host", back_populates="dkms_nodes")
    orr = relationship("ORR", back_populates="dkms_instances")
    tls_config = relationship("TLSConfigDKMS", back_populates="dkms_instances")
    agent_controllers = relationship(
        "AgentController", back_populates="dkms", cascade="all, delete"
    )


class AgentController(Base):
    __tablename__ = "agent_controller"

    id = Column(Integer, primary_key=True, autoincrement=True)
    id_dkms = Column(
        Integer,
        ForeignKey("dkms.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
        index=True,
    )
    id_sdn = Column(
        Integer,
        ForeignKey("sdn.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
        index=True,
    )
    id_host = Column(
        Integer,
        ForeignKey("host.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
    )

    dkms = relationship("DKMS", back_populates="agent_controllers")
    sdn = relationship("SDN", back_populates="agent_controllers")
    host = relationship("Host", back_populates="agent_controllers")
    # No borrar SAE al eliminar/recrear AgentController (p.ej. al editar topología).
    saes = relationship("SAE", back_populates="agent_controller")


class SAE(Base):
    __tablename__ = "sae"
    __table_args__ = (
        UniqueConstraint("simulation_id", "sae_id", name="uq_sae_simulation_sae_id"),
    )

    id = Column(Integer, primary_key=True, autoincrement=True)
    sae_id = Column(String(255), nullable=True, index=True)
    display_name = Column(String(255), nullable=True)
    sdn_id = Column(
        Integer,
        ForeignKey("sdn.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
        index=True,
    )
    tls_id = Column(
        Integer,
        ForeignKey("tls_config_sae.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
        index=True,
    )
    agent_dkms_id = Column(
        Integer,
        ForeignKey("agent_controller.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=True,
        index=True,
    )
    owner_user_id = Column(
        Integer,
        ForeignKey("user.id", onupdate="CASCADE", ondelete="SET NULL"),
        nullable=True,
        index=True,
    )
    simulation_id = Column(
        Integer,
        ForeignKey("simulation.id", onupdate="CASCADE", ondelete="SET NULL"),
        nullable=True,
        index=True,
    )
    dkms_id = Column(
        Integer,
        ForeignKey("dkms.id", onupdate="CASCADE", ondelete="SET NULL"),
        nullable=True,
        index=True,
    )
    status = Column(
        SAEnum(SaeStatusEnum, name="sae_status", values_callable=_enum_values),
        nullable=False,
        default=SaeStatusEnum.PENDING_CERT,
    )
    cert_serial = Column(String(128), nullable=True, index=True)
    cert_fingerprint = Column(String(128), nullable=True, index=True)
    cert_subject = Column(String(512), nullable=True)
    cert_not_before = Column(DateTime(timezone=True), nullable=True)
    cert_not_after = Column(DateTime(timezone=True), nullable=True)
    revoked_at = Column(DateTime(timezone=True), nullable=True)
    revocation_reason = Column(String(255), nullable=True)
    created_at = Column(DateTime(timezone=True), nullable=False, default=func.now())
    updated_at = Column(
        DateTime(timezone=True),
        nullable=False,
        default=func.now(),
        onupdate=func.now(),
    )

    sdn = relationship("SDN", back_populates="saes")
    tls_config = relationship("TLSConfigSAE", back_populates="saes")
    agent_controller = relationship("AgentController", back_populates="saes")
    owner_user = relationship("User")
    simulation = relationship("Simulation")
    dkms = relationship("DKMS")


class KME(Base):
    __tablename__ = "kme"

    id = Column(Integer, primary_key=True, autoincrement=True)
    local_qkc_id = Column(
        Integer,
        ForeignKey("qkc.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    neighbor_qkc_id = Column(
        Integer,
        ForeignKey("qkc.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    cert_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    key_id = Column(
        Integer,
        ForeignKey("data_file.id", onupdate="CASCADE", ondelete="NO ACTION"),
        nullable=False,
    )
    url_node_QKD = Column(String(512), nullable=False)
    neighbor_QKD = Column(String(255), nullable=False)
    etsi = Column(SAEnum(ETSIEnum, name="etsi", values_callable=_enum_values), nullable=False)
    channel_type = Column(
        SAEnum(ChannelTypeEnum, name="channel_type", values_callable=_enum_values),
        nullable=False,
        default=ChannelTypeEnum.QKD,
    )
    channel_distance = Column(Integer, nullable=False, default=0)
    channel_quditto_max_buffer_size = Column(Integer, nullable=False, default=100)
    channel_quditto_rate_r0 = Column(Float, nullable=False, default=2000.0)
    channel_quditto_rate_alpha = Column(Float, nullable=False, default=0.2)
    pqc_simulation = Column(Boolean, nullable=False, default=False)
    hybrid_enabled = Column(Boolean, nullable=False, default=False)
    pqc_kme_port = Column(Integer, nullable=False, default=6000)

    local_qkc = relationship("QKC", foreign_keys=[local_qkc_id], back_populates="local_kmes")
    neighbor_qkc = relationship("QKC", foreign_keys=[neighbor_qkc_id], back_populates="neighbor_kmes")
    cert = relationship("DataFile", foreign_keys=[cert_id])
    key = relationship("DataFile", foreign_keys=[key_id])
    token_bucket = relationship("TokenBucket", back_populates="kme", uselist=False, cascade="all, delete-orphan")


class TokenBucket(Base):
    __tablename__ = "token_bucket"

    id = Column(Integer, primary_key=True, autoincrement=True)
    kme_id = Column(
        Integer,
        ForeignKey("kme.id", onupdate="CASCADE", ondelete="CASCADE"),
        nullable=False,
        unique=True,
    )
    beta = Column(Float, nullable=True)
    gamma = Column(Float, nullable=True)
    delta_up = Column(Float, nullable=True)
    delta_down = Column(Float, nullable=True)
    T_obs = Column(Float, nullable=True)
    alpha0 = Column(Float, nullable=True)
    alpha_min = Column(Float, nullable=True)
    tau = Column(Float, nullable=True)
    B_min = Column(Integer, nullable=True)
    B_max = Column(Integer, nullable=True)
    R_max = Column(Integer, nullable=True)
    initial_tokens = Column(Integer, nullable=True)
    status_timeout = Column(Float, nullable=True)
    verify = Column(Boolean, nullable=True, default=False)

    kme = relationship("KME", back_populates="token_bucket")


class SimulationRun(Base):
    __tablename__ = "simulation_run"

    id = Column(Integer, primary_key=True, autoincrement=True)
    simulation_id = Column(
        Integer,
        ForeignKey("simulation.id", onupdate="CASCADE", ondelete="CASCADE"),
        nullable=False,
        index=True,
    )
    status = Column(
        SAEnum(
            SimulationRunStatusEnum,
            name="simulation_run_status",
            values_callable=_enum_values,
        ),
        nullable=False,
    )
    message = Column(Text, nullable=True)
    queued_at = Column(DateTime(timezone=True), nullable=True)
    started_at = Column(DateTime(timezone=True), nullable=True)
    finished_at = Column(DateTime(timezone=True), nullable=True)
    created_at = Column(DateTime(timezone=True), nullable=False, default=func.now())

    simulation = relationship("Simulation", back_populates="runs")
