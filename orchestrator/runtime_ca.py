from __future__ import annotations

import base64
import os
import threading
from datetime import datetime, timedelta, timezone
from typing import Optional

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID
from kubernetes import client, config
from kubernetes.client.exceptions import ApiException
from kubernetes.config.config_exception import ConfigException


_KUBE_CONFIG_LOCK = threading.Lock()
_KUBE_CONFIG_READY = False

RUNTIME_CA_SECRET_NAME = os.getenv("K8S_RUNTIME_MTLS_CA_SECRET", "sae-runtime-ca").strip() or "sae-runtime-ca"
CONTROL_NAMESPACE = (
    os.getenv("K8S_CONTROL_NAMESPACE")
    or os.getenv("NAMESPACE")
    or "dkms-main-ns"
).strip()


def _ensure_kube_config_loaded() -> None:
    global _KUBE_CONFIG_READY
    if _KUBE_CONFIG_READY:
        return
    with _KUBE_CONFIG_LOCK:
        if _KUBE_CONFIG_READY:
            return
        try:
            config.load_incluster_config()
        except ConfigException:
            config.load_kube_config()
        _KUBE_CONFIG_READY = True


def _core_v1_api() -> client.CoreV1Api:
    _ensure_kube_config_loaded()
    return client.CoreV1Api()


def _to_int_simulation_id(simulation_id: int | str) -> int:
    try:
        value = int(simulation_id)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"simulation_id invalido: {simulation_id!r}") from exc
    if value <= 0:
        raise ValueError("simulation_id debe ser mayor que cero")
    return value


def _control_secret_name(simulation_id: int) -> str:
    return f"{RUNTIME_CA_SECRET_NAME}-sim-{int(simulation_id)}"


def _decode_secret_value(raw: Optional[str]) -> str:
    if not raw:
        return ""
    try:
        return base64.b64decode(raw).decode("utf-8")
    except Exception:  # noqa: BLE001
        return ""


def _build_ca_pair(simulation_id: int) -> tuple[str, str]:
    now = datetime.now(timezone.utc)
    private_key = rsa.generate_private_key(public_exponent=65537, key_size=3072)
    subject = x509.Name(
        [
            x509.NameAttribute(NameOID.COUNTRY_NAME, "ES"),
            x509.NameAttribute(NameOID.ORGANIZATION_NAME, "DKMS Runtime CA"),
            x509.NameAttribute(NameOID.COMMON_NAME, f"{RUNTIME_CA_SECRET_NAME}-sim-{simulation_id}"),
        ]
    )
    certificate = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(subject)
        .public_key(private_key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(now - timedelta(minutes=5))
        .not_valid_after(now + timedelta(days=3650))
        .add_extension(x509.BasicConstraints(ca=True, path_length=0), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=False,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                key_cert_sign=True,
                crl_sign=True,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .sign(private_key=private_key, algorithm=hashes.SHA256())
    )
    cert_pem = certificate.public_bytes(serialization.Encoding.PEM).decode("utf-8")
    key_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    ).decode("utf-8")
    return cert_pem, key_pem


def get_or_create_runtime_ca_material(
    *,
    simulation_id: int | str,
    control_namespace: Optional[str] = None,
) -> tuple[str, str]:
    sim_id = _to_int_simulation_id(simulation_id)
    namespace = (control_namespace or CONTROL_NAMESPACE or "dkms-main-ns").strip()
    if not namespace:
        raise ValueError("control_namespace no puede estar vacio")

    secret_name = _control_secret_name(sim_id)
    core_v1_api = _core_v1_api()

    try:
        existing = core_v1_api.read_namespaced_secret(name=secret_name, namespace=namespace)
    except ApiException as exc:
        if exc.status != 404:
            raise
        existing = None

    if existing is not None:
        data = getattr(existing, "data", None) or {}
        cert_pem = _decode_secret_value(data.get("ca.crt"))
        key_pem = _decode_secret_value(data.get("ca.key"))
        if cert_pem and key_pem:
            return cert_pem, key_pem

    cert_pem, key_pem = _build_ca_pair(sim_id)
    body = client.V1Secret(
        metadata=client.V1ObjectMeta(name=secret_name),
        type="Opaque",
        string_data={"ca.crt": cert_pem, "ca.key": key_pem},
    )
    try:
        core_v1_api.create_namespaced_secret(namespace=namespace, body=body)
        return cert_pem, key_pem
    except ApiException as exc:
        if exc.status != 409:
            raise
        current = core_v1_api.read_namespaced_secret(name=secret_name, namespace=namespace)
        data = getattr(current, "data", None) or {}
        current_cert = _decode_secret_value(data.get("ca.crt"))
        current_key = _decode_secret_value(data.get("ca.key"))
        if current_cert and current_key:
            return current_cert, current_key
        raise RuntimeError(
            f"Secret {namespace}/{secret_name} existe pero no contiene ca.crt/ca.key validos"
        )


def sync_runtime_ca_to_simulation_namespace(
    *,
    simulation_id: int | str,
    simulation_namespace: int | str,
    simulation_secret_name: Optional[str] = None,
    control_namespace: Optional[str] = None,
) -> str:
    cert_pem, _ = get_or_create_runtime_ca_material(
        simulation_id=simulation_id,
        control_namespace=control_namespace,
    )

    namespace = str(simulation_namespace).strip()
    if not namespace:
        raise ValueError("simulation_namespace no puede estar vacio")
    secret_name = (simulation_secret_name or RUNTIME_CA_SECRET_NAME).strip() or "sae-runtime-ca"

    core_v1_api = _core_v1_api()
    body = client.V1Secret(
        metadata=client.V1ObjectMeta(name=secret_name),
        type="Opaque",
        string_data={"ca.crt": cert_pem},
    )
    try:
        core_v1_api.create_namespaced_secret(namespace=namespace, body=body)
        return cert_pem
    except ApiException as exc:
        if exc.status != 409:
            raise

    current = core_v1_api.read_namespaced_secret(name=secret_name, namespace=namespace)
    current_data = getattr(current, "data", None) or {}
    current_cert = _decode_secret_value(current_data.get("ca.crt"))
    if current_cert == cert_pem:
        return cert_pem

    metadata = client.V1ObjectMeta(name=secret_name, resource_version=getattr(current.metadata, "resource_version", None))
    replace_body = client.V1Secret(
        metadata=metadata,
        type="Opaque",
        string_data={"ca.crt": cert_pem},
    )
    core_v1_api.replace_namespaced_secret(name=secret_name, namespace=namespace, body=replace_body)
    return cert_pem
