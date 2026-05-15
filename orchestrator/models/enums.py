from __future__ import annotations

from enum import Enum


class ETSIType(str, Enum):
    ETSI_014 = "ETSI_014"
    ETSI_004 = "ETSI_004"


class CipherDKMS(str, Enum):
    TLS_AES_256_GCM_SHA384 = "TLS_AES_256_GCM_SHA384"
    TLS_CHACHA20_POLY1305_SHA256 = "TLS_CHACHA20_POLY1305_SHA256"
    TLS_AES_128_GCM_SHA256 = "TLS_AES_128_GCM_SHA256"


class TLSVersion(str, Enum):
    TLSV1_3 = "TLSv1_3"
    TLSV1_2 = "TLSv1_2"


class SimulationStatus(str, Enum):
    PENDING = "pending"
    RUNNING = "running"
    FINISHED = "finished"
    ERROR = "error"

class HTTPType(str, Enum):
    HTTP = "http"
    HTTPS = "https"

class ChannelType(str,Enum):
    PQC_SIMULATION = "pqc-simulation"
    QKD = "qkd"


class SaeStatus(str, Enum):
    PENDING_CERT = "pending_cert"
    ACTIVE = "active"
    REVOKED = "revoked"
    EXPIRED = "expired"
