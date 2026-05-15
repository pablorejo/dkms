"""Tests del path opt-in de sidecars Rust (orr + qkc) en PodDKMS.

Estos tests validan los helpers nuevos sin invocar k8s. Toda la
construcción de containers se hace en memoria; la conexión real al API
de k8s sólo ocurre en ``crear_config_map`` / ``create_pod`` cuando se
llama desde ``_deploy_pod``, así que cubrimos:

* import + lectura de constantes (``K8S_ORR_IMAGE`` etc.)
* generación de ``qkc.toml`` desde la topología
* ``ORR_*`` env vars
* overrides ``DKMS_*`` para southbound + listen + tls
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.append(str(ROOT))

import importlib
import pytest


@pytest.fixture
def sidecars_enabled(monkeypatch):
    """Activa K8S_DKMS_RUST_SIDECARS y recarga ``pods`` para que coja el flag."""
    monkeypatch.setenv("K8S_DKMS_RUST_SIDECARS", "true")
    import pods
    importlib.reload(pods)
    return pods


def test_module_constants_default_off():
    """En el path por defecto los sidecars están desactivados."""
    # Ensure clean env
    os.environ.pop("K8S_DKMS_RUST_SIDECARS", None)
    import pods
    importlib.reload(pods)
    assert pods.K8S_DKMS_RUST_SIDECARS is False
    assert pods.K8S_ORR_IMAGE.startswith("docker.io/pablopio/orr:")
    assert pods.K8S_QKC_IMAGE.startswith("docker.io/pablopio/qkc:")


def test_module_constants_with_flag(sidecars_enabled):
    pods = sidecars_enabled
    assert pods.K8S_DKMS_RUST_SIDECARS is True
    assert pods.K8S_ORR_GRPC_PORT == 50052
    assert pods.K8S_QKC_GRPC_PORT == 50051
    assert pods.K8S_QKC_TCP_PORT == 7001


class _PodStub:
    """Stub mínimo que expone los helpers sin invocar k8s."""

    def __init__(self, pods_module, model_dkms, model_name="dkms-1", suffix="1"):
        self._pods = pods_module
        self.model = model_dkms
        self.name = model_name
        self.name_suffix = suffix
        # Atributos que necesitan los helpers que vienen del PodDKMS init.
        # Los ponemos a None / vacíos: las helpers que probamos no los
        # tocan, sólo ``create_pod``.
        self._quditto_volume_name = "quditto-config"
        self._quditto_cert_volume_name = "quditto-certs"
        self._quditto_config_map_name = f"{model_name}-quditto"
        # Forzamos el método de PodDKMS apuntando ``self`` como instancia
        # vía mocks de los métodos privados sin tocar k8s.

    # Métodos delegados a las helpers de PodDKMS:
    def __getattr__(self, name):
        method = getattr(self._pods.PodDKMS, name, None)
        if method is None:
            raise AttributeError(name)
        # llama al unbound method pasando self como instancia
        return method.__get__(self)


def _build_dkms_model_with_qkd_neighbor():
    """Construye un ModelDKMS válido con 1 vecino QKD."""
    from models import (
        Channel,
        ChannelType,
        ETSIType,
        KMEConfig,
        ModelDKMS,
        ModelFile,
        ModelHost,
        ModelORR,
        ModelQKC,
        TokenBucketConfig,
    )

    host_dkms = ModelHost(id=1, id_simulation=1, ip="dkms-1", port=8443)
    host_orr = ModelHost(id=2, id_simulation=1, ip="dkms-1", port=50052)
    host_qkc = ModelHost(id=3, id_simulation=1, ip="dkms-1", port=50051)
    host_qkc_neighbor = ModelHost(id=4, id_simulation=1, ip="dkms-2", port=50051)

    kme = KMEConfig(
        local_qkc_id=1,
        local_qkc_ip=host_qkc.ip,
        neighbor_qkc_id=2,
        neighbor_qkc_ip=host_qkc_neighbor.ip,
        neighbor_qkc_port=host_qkc_neighbor.port,
        neighbor_qkd_id="QKD_2",
        local_qkd_id="QKD_1",
        local_url_node_qkd="https://kme.local",
        etsi=ETSIType.ETSI_004,
        cert=ModelFile(path="certs/kme.crt"),
        key=ModelFile(path="certs/kme.key"),
        token_bucket=TokenBucketConfig(beta=0.1, gamma=0.2, delta_up=0.3),
        channel=Channel(
            type_channel=ChannelType.QKD,
            distance=5,
            ttl=600,
            max_buffer_size=500,
        ),
    )

    qkc = ModelQKC(
        id=1,
        id_host=1,
        kme_host="kme-1",
        host=host_qkc,
        kmes=[kme],
    )
    orr = ModelORR(
        id=1,
        id_host=1,
        qkc_id=1,
        host=host_orr,
        qkc=qkc,
    )
    return ModelDKMS(
        id=1,
        id_host=1,
        orr_id=1,
        host=host_dkms,
        orr=orr,
    )


def test_qkc_runtime_toml_contains_neighbor(sidecars_enabled):
    pods = sidecars_enabled
    model = _build_dkms_model_with_qkd_neighbor()
    stub = _PodStub(pods, model)
    toml = stub._qkc_runtime_toml()

    assert "qkc_id       = 1" in toml
    assert 'peer_listen  = "0.0.0.0:7001"' in toml
    assert 'local_listen = "0.0.0.0:7100"' in toml
    assert "[[links]]" in toml
    assert "neighbor_id        = 2" in toml
    assert 'neighbor_peer_addr = "dkms-2:7001"' in toml
    # key_size múltiplo de 8 (KMEConfig default 256)
    assert "key_size_bits      = 256" in toml


def test_orr_runtime_env_pointing_localhost(sidecars_enabled):
    pods = sidecars_enabled
    model = _build_dkms_model_with_qkd_neighbor()
    stub = _PodStub(pods, model)
    env = stub._orr_runtime_env()

    assert env["ORR_GRPC_ADDR"] == "0.0.0.0:50052"
    assert env["ORR_METRICS_ADDR"] == "0.0.0.0:9101"
    assert env["ORR_QKC_LOCAL_ADDR"] == "127.0.0.1:7100"
    assert env["ORR_QKC_ID"] == "1"
    assert env["RUST_LOG"]


def test_dkms_rust_env_overrides_localhost(sidecars_enabled):
    pods = sidecars_enabled
    model = _build_dkms_model_with_qkd_neighbor()
    stub = _PodStub(pods, model)
    env = stub._dkms_rust_env_overrides(sdn_host="sdn-99", sdn_port="50053")

    # Southbound hacia sidecars locales (mismo Pod, network ns compartido).
    assert env["DKMS_SOUTHBOUND__QKC_ENDPOINT"] == "http://127.0.0.1:50051"
    assert env["DKMS_SOUTHBOUND__ORR_ENDPOINT"] == "http://127.0.0.1:50052"
    assert env["DKMS_SOUTHBOUND__SDN_ENDPOINT"] == "http://sdn-99:50053"
    # Listen en el puerto SAE del modelo + 1 para peer.
    assert env["DKMS_LISTEN__SAE_ADDR"] == "0.0.0.0:8443"
    assert env["DKMS_LISTEN__PEER_ADDR"] == "0.0.0.0:8444"
    assert env["DKMS_LISTEN__GRPC_ADDR"] == "0.0.0.0:50054"
    # TLS material apunta al volumen que rellena el sidecar Quditto.
    assert env["DKMS_TLS__CERT_PATH"] == "/app/certs/server.crt"
    assert env["DKMS_TLS__SAE_CLIENT_CA"] == "/app/certs/ca.crt"


def test_dkms_rust_env_overrides_fallback_sdn(sidecars_enabled):
    pods = sidecars_enabled
    model = _build_dkms_model_with_qkd_neighbor()
    stub = _PodStub(pods, model)
    env = stub._dkms_rust_env_overrides(sdn_host=None, sdn_port=None)
    # Fallbacks razonables cuando el orchestator no resolvió.
    assert env["DKMS_SOUTHBOUND__SDN_ENDPOINT"] == "http://sdn:50053"
