import base64
import json
import os
import re
from pathlib import Path
from typing import Any, Dict, Optional

from kubernetes import config, client
from kubernetes.client.exceptions import ApiException
from kubernetes.config.config_exception import ConfigException

from models import ChannelType, Model, ModelDKMS, ModelSDN

DOCKER_HUB_TOKEN = os.getenv("DOCKER_HUB_TOKEN")
DOCKER_HUB_USERNAME = os.getenv("DOCKER_HUB_USERNAME")
DOCKER_HUB_EMAIL = os.getenv("DOCKER_HUB_EMAIL")
DOCKER_HUB_SERVER = os.getenv("DOCKER_HUB_SERVER", "https://index.docker.io/v1/")
DOCKER_HUB_SECRET_NAME = os.getenv("DOCKER_HUB_SECRET_NAME", "dockerhub-pull")
K8S_IMAGE_PULL_POLICY = os.getenv("K8S_IMAGE_PULL_POLICY", "IfNotPresent")
K8S_CONFIG_MODE = os.getenv("K8S_CONFIG_MODE", "image").lower()
K8S_CONFIG_FILES_DIR = os.getenv("K8S_CONFIG_FILES_DIR")
K8S_INGRESS_ENABLED = os.getenv("K8S_INGRESS_ENABLED", "true").lower() in {"1", "true", "yes"}
K8S_INGRESS_HOST = os.getenv("K8S_INGRESS_HOST", "dkms.uvigo.com")
K8S_RUNTIME_INGRESS_HOST = os.getenv("K8S_RUNTIME_INGRESS_HOST", K8S_INGRESS_HOST)
K8S_INGRESS_AUTH_URL = os.getenv(
    "K8S_INGRESS_AUTH_URL",
    "http://authz.dkms-main-ns.svc.cluster.local:8081/authorize",
)
K8S_INGRESS_AUTH_HEADERS = os.getenv(
    "K8S_INGRESS_AUTH_HEADERS", "X-User-Id,X-Simulation-Id"
)
K8S_INGRESS_CLASS = os.getenv("K8S_INGRESS_CLASS", "nginx")
K8S_RUNTIME_MTLS_ENABLED = os.getenv("K8S_RUNTIME_MTLS_ENABLED", "true").lower() in {
    "1",
    "true",
    "yes",
}
K8S_RUNTIME_MTLS_CA_SECRET = os.getenv("K8S_RUNTIME_MTLS_CA_SECRET", "sae-runtime-ca")
K8S_RUNTIME_MTLS_VERIFY_DEPTH = os.getenv("K8S_RUNTIME_MTLS_VERIFY_DEPTH", "2")
K8S_RUNTIME_SSL_CIPHERS = os.getenv(
    "K8S_RUNTIME_SSL_CIPHERS",
    "TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:TLS_AES_128_GCM_SHA256",
)
K8S_INGRESS_TEMPLATES_DIR = os.getenv("K8S_INGRESS_TEMPLATES_DIR")
K8S_QUDITTO_IMAGE = os.getenv("QUDITTO_IMAGE", "docker.io/pablopio/simple-quditto:v1")
K8S_QUDITTO_PORT = int(os.getenv("QUDITTO_PORT", "5000"))
K8S_QUDITTO_METRICS_PORT = int(os.getenv("QUDITTO_METRICS_PORT", "8000"))
K8S_LOADTEST_METRICS_PORT = int(os.getenv("K8S_LOADTEST_METRICS_PORT", "9095"))
K8S_LOADTEST_IMAGE = os.getenv("K8S_LOADTEST_IMAGE", "docker.io/pablopio/dkms-loadtest:v1")
K8S_LOADTEST_SERVICE_NAME = "loadtest-runners"
K8S_QUDITTO_DEFAULT_TTL = int(os.getenv("QUDITTO_DEFAULT_TTL", "600"))
K8S_QUDITTO_VERBOSE = os.getenv("QUDITTO_VERBOSE", "INFO")
# v2.4.6: 4 workers uvicorn del sidecar (revertido de 8). Diagnóstico:
# con 8 workers + cliente decrypt keepalive=4, las 4 conexiones reusables
# del cliente solo cubrían ~3 de los 8 workers (probabilidad de cubrir
# los 8 con 4 conexiones aleatorias ~41%). Los 5 workers restantes
# quedaban idle. Bajar a 4 garantiza que keepalive=4 cubra cada worker
# en SO_REUSEPORT distribute, eliminando connection affinity.
# Validado: buf_generated subió 14→40 keys/s/DKMS (+185%) tras el cambio.
K8S_QUDITTO_WORKERS = os.getenv("QUDITTO_WORKERS", "8")
try:
    # v2.5.2: cap del buffer reducido a 500 (era 100 default pero el
    # cluster venía corriendo con override 10000). Con cap=10000 los
    # buffers tardan ~30 min en llenarse al ritmo de 5 keys/s/buffer
    # (la SDN nunca ve transición empty→full→empty completa). Con
    # cap=500 el ciclo dura ~100 s, aparecen buffers SAT y HIGH, la
    # SDN puede ejercer su lógica QoS bidireccional v2.4.3 (redistribuir
    # capacity de saturados a vacíos). Validado cluster: distribución
    # mejora de 90% empty → 73% empty con LOW/MID/HIGH/SAT visibles.
    K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE = max(1, int(os.getenv("QUDITTO_MAX_BUFFER_SIZE", "500")))
except (TypeError, ValueError):
    K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE = 100
try:
    K8S_QUDITTO_DEFAULT_RATE_R0 = float(os.getenv("QUDITTO_RATE_R0", "2000.0"))
    if K8S_QUDITTO_DEFAULT_RATE_R0 <= 0:
        K8S_QUDITTO_DEFAULT_RATE_R0 = 2000.0
except (TypeError, ValueError):
    K8S_QUDITTO_DEFAULT_RATE_R0 = 2000.0
try:
    K8S_QUDITTO_DEFAULT_RATE_ALPHA = float(os.getenv("QUDITTO_RATE_ALPHA", "0.2"))
    if K8S_QUDITTO_DEFAULT_RATE_ALPHA < 0:
        K8S_QUDITTO_DEFAULT_RATE_ALPHA = 0.2
except (TypeError, ValueError):
    K8S_QUDITTO_DEFAULT_RATE_ALPHA = 0.2
# v2.4.5: HTTP plano para el sidecar local (mismo pod, localhost). El TLS
# añadía handshake + cifrado per-request sin beneficio de seguridad
# (tráfico nunca sale del pod). Bench cluster: con TLS el sidecar tarda
# 5650 ms/batch, sin TLS esperado <100 ms. Activar/desactivar con
# QUDITTO_INSECURE (default "1" = HTTP plano).
K8S_QUDITTO_INSECURE = os.getenv("QUDITTO_INSECURE", "1").lower() not in ("0", "false", "no")
_qkc_scheme = "http" if K8S_QUDITTO_INSECURE else "https"
K8S_QUDITTO_LOCAL_URL = os.getenv(
    "QUDITTO_LOCAL_URL", f"{_qkc_scheme}://127.0.0.1:{K8S_QUDITTO_PORT}",
)
K8S_QUDITTO_CLIENT_CERT_PATH = os.getenv("QUDITTO_CLIENT_CERT_PATH", "/app/certs/client.crt")
K8S_QUDITTO_CLIENT_KEY_PATH = os.getenv("QUDITTO_CLIENT_KEY_PATH", "/app/certs/client.key")

# ─── Rust per-module sidecars (opt-in v3.4) ──────────────────────────────────
K8S_DKMS_RUST_SIDECARS = os.getenv("K8S_DKMS_RUST_SIDECARS", "false").lower() in {
    "1", "true", "yes",
}
K8S_ORR_IMAGE = os.getenv("ORR_IMAGE", "docker.io/pablopio/orr:v2")
K8S_QKC_IMAGE = os.getenv("QKC_IMAGE", "docker.io/pablopio/qkc:v2")
K8S_ORR_GRPC_PORT = int(os.getenv("ORR_GRPC_PORT", "50052"))
K8S_ORR_METRICS_PORT = int(os.getenv("ORR_METRICS_PORT", "9101"))
K8S_QKC_GRPC_PORT = int(os.getenv("QKC_GRPC_PORT", "50051"))
K8S_QKC_TCP_PORT = int(os.getenv("QKC_TCP_PORT", "7001"))
K8S_QKC_METRICS_PORT = int(os.getenv("QKC_METRICS_PORT", "9100"))
K8S_DKMS_RUST_GRPC_PORT = int(os.getenv("DKMS_RUST_GRPC_PORT", "50054"))
K8S_DKMS_RUST_METRICS_PORT = int(os.getenv("DKMS_RUST_METRICS_PORT", "9103"))
K8S_DKMS_RUST_LOG = os.getenv("DKMS_RUST_LOG", "info")
K8S_ORR_RUST_LOG = os.getenv("ORR_RUST_LOG", "info")
K8S_QKC_RUST_LOG = os.getenv("QKC_RUST_LOG", "info")
# Timeout HTTP del DKMS para solicitudes KME/QKD.
K8S_DKMS_KME_HTTP_TIMEOUT_SECONDS = os.getenv("KME_HTTP_TIMEOUT_SECONDS", "130")
K8S_DKMS_KME_ENC_KEYS_REQUEST_TIMEOUT_SECONDS = os.getenv(
    "KME_ENC_KEYS_REQUEST_TIMEOUT_SECONDS", "130"
)
K8S_DKMS_KME_ENC_KEYS_RETRY_WINDOW_SECONDS = os.getenv(
    "KME_ENC_KEYS_RETRY_WINDOW_SECONDS", "30"
)
K8S_DKMS_QKC_RELAY_FORWARD_TIMEOUT_SECONDS = os.getenv(
    "QKC_RELAY_FORWARD_TIMEOUT_SECONDS", "60"
)
K8S_DKMS_QKC_SEND_TIMEOUT_SECONDS = os.getenv(
    "QKC_SEND_TIMEOUT_SECONDS", "120"
)
K8S_DKMS_PQC_SOCKET_TIMEOUT_SECONDS = os.getenv(
    "PQC_SOCKET_TIMEOUT_SECONDS", "20"
)
K8S_DKMS_KME_ENC_KEYS_RETRY_INTERVAL_SECONDS = os.getenv(
    "KME_ENC_KEYS_RETRY_INTERVAL_SECONDS", "0.5"
)
K8S_DKMS_KME_DEC_KEYS_RETRY_WINDOW_SECONDS = os.getenv(
    "KME_DEC_KEYS_RETRY_WINDOW_SECONDS", "15"
)
K8S_DKMS_KME_DEC_KEYS_RETRY_INTERVAL_SECONDS = os.getenv(
    "KME_DEC_KEYS_RETRY_INTERVAL_SECONDS", "0.5"
)
K8S_DKMS_QKC_DEC_KEYS_RETRY_WINDOW_SECONDS = os.getenv(
    "QKC_DEC_KEYS_RETRY_WINDOW_SECONDS",
    K8S_DKMS_KME_DEC_KEYS_RETRY_WINDOW_SECONDS,
)
try:
    _k8s_qkc_dec_keys_retry_window_value = float(K8S_DKMS_QKC_DEC_KEYS_RETRY_WINDOW_SECONDS)
    if _k8s_qkc_dec_keys_retry_window_value <= 0:
        _k8s_qkc_dec_keys_retry_window_value = 15.0
except (TypeError, ValueError):
    _k8s_qkc_dec_keys_retry_window_value = 15.0
K8S_DKMS_QKC_SERVER_DECRYPT_RETRY_WINDOW_SECONDS = os.getenv(
    "QKC_SERVER_DECRYPT_RETRY_WINDOW_SECONDS",
    str(max(60.0, _k8s_qkc_dec_keys_retry_window_value * 4.0)).rstrip("0").rstrip("."),
)
K8S_DKMS_EXT_KEYS_RETRY_WINDOW_SECONDS = os.getenv(
    "DKMS_EXT_KEYS_RETRY_WINDOW_SECONDS", "600"
)
K8S_DKMS_EXT_KEYS_RETRY_INTERVAL_SECONDS = os.getenv(
    "DKMS_EXT_KEYS_RETRY_INTERVAL_SECONDS", "0.5"
)
K8S_DKMS_REQUIRE_CLIENT_CERT_IDENTITY = os.getenv(
    "DKMS_REQUIRE_CLIENT_CERT_IDENTITY", "true"
)
K8S_DKMS_REQUIRE_DEC_KEYS_PATH_MATCH = os.getenv(
    "DKMS_REQUIRE_DEC_KEYS_PATH_MATCH", "true"
)
K8S_DKMS_TRUSTED_PROXY_IPS = os.getenv("DKMS_TRUSTED_PROXY_IPS", "")
K8S_DKMS_SAE_BUFFER_OBSERVATION_WINDOW_SECONDS = os.getenv(
    "DKMS_SAE_BUFFER_OBSERVATION_WINDOW_SECONDS", "10"
)
K8S_DKMS_SAE_BUCKET_RETRY_AFTER_CEILING_SECONDS = os.getenv(
    "DKMS_SAE_BUCKET_RETRY_AFTER_CEILING_SECONDS", "3"
)
K8S_DKMS_GENERATOR_REFILL_DEMAND_KEYS_PER_SECOND = os.getenv(
    "DKMS_GENERATOR_REFILL_DEMAND_KEYS_PER_SECOND", "1.0"
)
K8S_DKMS_GENERATOR_MAX_CONCURRENT_GENERATIONS = os.getenv(
    "DKMS_GENERATOR_MAX_CONCURRENT_GENERATIONS", "60"
)
# Tamaño del pool de envío async del Generator (desacopla pacer de la
# latencia ORR→QKC→HTTP).
K8S_DKMS_ASYNC_SEND_WORKERS = os.getenv("DKMS_ASYNC_SEND_WORKERS", "500")
# KME: prefetch N enc_keys de Quditto en background para evitar que
# cada send espere un round-trip HTTP al backend.
# Default 128 (validado en cluster 50 DKMSs). Combinado con
# QKC_DECRYPT_PROCESS_WORKERS=16, el pool de prefetch alimenta al hot
# path sin que el sender espere; con count=64 el ratio ok/err del POST
# era 33/372 (8%); con count=128 sube a 39/185 (17%).
K8S_KME_ENC_PREFETCH_COUNT = os.getenv("KME_ENC_PREFETCH_COUNT", "512")
# Batch HTTP al pedir claves a Quditto: N claves por request. Amortiza
# RTT entre N claves. Default 64 (validado v2.3.7 en cluster 50 DKMSs):
# bench directo al sidecar muestra que pasar de batch=8 a batch=64
# multiplica el throughput 5× (324 → 1750 keys/s/link). En cluster
# real: +18% fill_rate, -27% POST err, +78% buffers activos.
K8S_KME_ENC_KEYS_BATCH_SIZE = os.getenv("KME_ENC_KEYS_BATCH_SIZE", "64")
# Cap de claves en vuelo (pre-ACK) por peer. Tspec × RTT_ACK da el
# tamaño mínimo; con cientos de keys/s y RTT de segundos conviene
# dejar varios miles de slots.
K8S_DKMS_MAX_IN_FLIGHT_KEYS_PER_PEER = os.getenv(
    "DKMS_MAX_IN_FLIGHT_KEYS_PER_PEER", "2000"
)
# Deadline de ACK antes de considerar clave perdida.
K8S_DKMS_ACK_TIMEOUT_SECONDS = os.getenv("DKMS_ACK_TIMEOUT_SECONDS", "30")
K8S_QKC_ENABLE_TOKEN_BUCKET = os.getenv("QKC_ENABLE_TOKEN_BUCKET", "true")
# Intervalo de poll del scheduler del Generator de buffers. La tasa real
# de producción la gobierna el token bucket del QKC (modelo Quditto) vía
# reserve_link_capacity; esta variable solo actúa como suelo del bucle.
K8S_DKMS_BUFFER_GENERATION_INTERVAL_SECONDS = os.getenv(
    "DKMS_BUFFER_GENERATION_INTERVAL_SECONDS", "0.1"
)
K8S_OBSERVABILITY_ENABLED = os.getenv("K8S_OBSERVABILITY_ENABLED", "true").lower() in {
    "1",
    "true",
    "yes",
}
K8S_OBS_PROMETHEUS_IMAGE = os.getenv(
    "K8S_OBS_PROMETHEUS_IMAGE", "prom/prometheus:v2.54.1"
)
K8S_OBS_LOKI_IMAGE = os.getenv("K8S_OBS_LOKI_IMAGE", "grafana/loki:2.9.8")
K8S_OBS_GRAFANA_IMAGE = os.getenv(
    "K8S_OBS_GRAFANA_IMAGE", "grafana/grafana:11.1.0"
)
K8S_OBS_PROMTAIL_IMAGE = os.getenv(
    "K8S_OBS_PROMTAIL_IMAGE", "grafana/promtail:2.9.8"
)
K8S_OBS_PROM_RETENTION = os.getenv("K8S_OBS_PROM_RETENTION", "72h")
K8S_OBS_LOKI_RETENTION = os.getenv("K8S_OBS_LOKI_RETENTION", "168h")
K8S_OBS_GRAFANA_PORT = int(os.getenv("K8S_OBS_GRAFANA_PORT", "3000"))
K8S_OBS_GRAFANA_ANON_ROLE = os.getenv("K8S_OBS_GRAFANA_ANON_ROLE", "Editor")
K8S_OBS_PROMETHEUS_PORT = int(os.getenv("K8S_OBS_PROMETHEUS_PORT", "9090"))
K8S_OBS_LOKI_PORT = int(os.getenv("K8S_OBS_LOKI_PORT", "3100"))
K8S_OBS_GRAFANA_PROMETHEUS_UID = "prometheus"
K8S_OBS_GRAFANA_LOKI_UID = "loki"
K8S_OBS_PERSISTENCE_ENABLED = os.getenv(
    "K8S_OBS_PERSISTENCE_ENABLED", "false"
).lower() in {"1", "true", "yes"}
K8S_OBS_STORAGE_CLASS = os.getenv("K8S_OBS_STORAGE_CLASS", "").strip()
K8S_OBS_PROM_PVC_SIZE = os.getenv("K8S_OBS_PROM_PVC_SIZE", "30Gi")
K8S_OBS_LOKI_PVC_SIZE = os.getenv("K8S_OBS_LOKI_PVC_SIZE", "50Gi")
K8S_OBS_GRAFANA_PVC_SIZE = os.getenv("K8S_OBS_GRAFANA_PVC_SIZE", "10Gi")
K8S_PROTECT_SIM_PODS = os.getenv("K8S_PROTECT_SIM_PODS", "true").lower() in {
    "1",
    "true",
    "yes",
}
K8S_DO_NOT_DISRUPT_ANNOTATION_KEY = os.getenv(
    "K8S_DO_NOT_DISRUPT_ANNOTATION_KEY",
    "karpenter.sh/do-not-disrupt",
).strip()


def _parse_bool(value: Any, default: bool = False) -> bool:
    if value is None:
        return default
    if isinstance(value, bool):
        return value
    if isinstance(value, (int, float)):
        return bool(value)
    if isinstance(value, str):
        lowered = value.strip().lower()
        if lowered in {"1", "true", "yes", "y", "on"}:
            return True
        if lowered in {"0", "false", "no", "n", "off"}:
            return False
    return default


def _ingress_base_url() -> str:
    host = (K8S_INGRESS_HOST or "").strip()
    if not host or host == "*":
        return "%(protocol)s://%(domain)s"
    if host.startswith(("http://", "https://")):
        return host.rstrip("/")
    return f"http://{host}"


def _normalize_channel_type(value: Any) -> str:
    if value is None:
        return ""
    raw = str(value).strip().lower().replace("_", "-")
    if raw.endswith(".qkd"):
        return ChannelType.QKD.value
    if raw.endswith(".pqc-simulation"):
        return ChannelType.PQC_SIMULATION.value
    if raw in {"qkd"}:
        return ChannelType.QKD.value
    if raw in {"pqc-simulation", "pqc_simulation", "simulated", "simulation"}:
        return ChannelType.PQC_SIMULATION.value
    return raw


def _is_qkd_link(kme_payload: Dict[str, Any]) -> bool:
    if _parse_bool(kme_payload.get("pqc_simulation"), default=False):
        return False

    channel = kme_payload.get("channel")
    if isinstance(channel, dict):
        channel_type = _normalize_channel_type(channel.get("type_channel"))
        if channel_type == ChannelType.QKD.value:
            return True
        if channel_type:
            return False

    # Retrocompatibilidad para configuraciones antiguas sin campo channel.
    return True


def _compute_quditto_role(local_qkc_id: Any, neighbor_qkc_id: Any) -> str:
    try:
        return "initiator" if int(local_qkc_id) < int(neighbor_qkc_id) else "responder"
    except (TypeError, ValueError):
        return "responder"


def _safe_int(value: Any) -> Optional[int]:
    try:
        return int(value)
    except (TypeError, ValueError):
        return None


def _safe_positive_int(value: Any, default: int) -> int:
    parsed = _safe_int(value)
    if parsed is None or parsed < 1:
        return int(default)
    return int(parsed)


def _safe_nonnegative_float(value: Any, default: float) -> float:
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return float(default)
    if parsed < 0:
        return float(default)
    return float(parsed)


def _safe_positive_float(value: Any, default: float) -> float:
    parsed = _safe_nonnegative_float(value, default)
    if parsed <= 0:
        return float(default)
    return float(parsed)


def _infer_node_id_from_ip(ip_value: Any) -> Optional[int]:
    if not isinstance(ip_value, str):
        return None
    chunks = ip_value.strip().split(".")
    if len(chunks) != 4:
        return None
    octet = _safe_int(chunks[-1])
    if octet is None or octet <= 100:
        return None
    inferred = octet - 100
    if inferred <= 0:
        return None
    return inferred


def _infer_dkms_service_suffix(value: Any) -> Optional[int]:
    if not isinstance(value, str):
        return None
    match = re.match(r"^dkms-(\d+)(?:[.:/].*)?$", value.strip(), re.IGNORECASE)
    if not match:
        return None
    suffix = _safe_int(match.group(1))
    if suffix is None or suffix <= 0:
        return None
    return suffix


def _is_legacy_host_pattern(host_id: int, node_id: int) -> bool:
    return int(host_id) == (int(node_id) * 10 + 3)


def _resolve_neighbor_qkc_service_suffix(
    *,
    local_host_id: int | None,
    local_node_id: int,
    neighbor_node_id: int,
    raw_neighbor_qkc_ip: Any,
) -> Optional[int]:
    legacy_candidate = int(neighbor_node_id) * 10 + 3
    runtime_candidate: Optional[int] = None
    if local_host_id is not None:
        delta = int(neighbor_node_id) - int(local_node_id)
        runtime_candidate = int(local_host_id) + delta * 3
        if runtime_candidate <= 0:
            runtime_candidate = None

    if local_host_id is not None and _is_legacy_host_pattern(int(local_host_id), int(local_node_id)):
        return legacy_candidate if legacy_candidate > 0 else None

    declared_suffix = _infer_dkms_service_suffix(raw_neighbor_qkc_ip)
    if declared_suffix is not None:
        if runtime_candidate is not None and declared_suffix == runtime_candidate:
            return declared_suffix
        if declared_suffix == legacy_candidate:
            return declared_suffix
        if runtime_candidate is not None:
            return runtime_candidate
        return declared_suffix

    if runtime_candidate is not None:
        return runtime_candidate
    if legacy_candidate > 0:
        return legacy_candidate
    return None


def _canonicalize_runtime_dkms_payload(payload: Dict[str, Any]) -> Dict[str, Any]:
    """Normaliza IDs de enrutado DKMS/ORR/QKC para alinearlos con SDN canónico.

    La simulación en BD puede usar IDs runtime (p.ej. DKMS=25, ORR=25, QKC=25),
    pero SDN expone rutas y bindings con IDs canónicos (1..N).
    Esta normalización evita desalineaciones en get_path_to/QKC y en KME neighbors.
    """
    normalized: Dict[str, Any] = json.loads(json.dumps(payload, ensure_ascii=False))

    # En K8s el mTLS de cliente se termina en ingress runtime.
    # Forzamos backend DKMS en HTTP interno para evitar 502 por mismatch HTTP/HTTPS
    # cuando el payload runtime incluye "tls" heredado de configuraciones legacy.
    normalized.pop("tls", None)

    host_payload = normalized.get("host")
    local_ip = host_payload.get("ip") if isinstance(host_payload, dict) else None
    local_node_id = _infer_node_id_from_ip(local_ip)
    if local_node_id is None:
        return normalized

    local_host_id = _safe_int(normalized.get("id_host"))
    if local_host_id is None and isinstance(host_payload, dict):
        local_host_id = _safe_int(host_payload.get("id"))

    runtime_service_name: Optional[str] = None
    if local_host_id is not None and local_host_id > 0:
        runtime_service_name = f"dkms-{int(local_host_id)}"
        if isinstance(host_payload, dict):
            host_payload["ip"] = runtime_service_name

    normalized["id"] = int(local_node_id)
    normalized["orr_id"] = int(local_node_id)

    orr_payload = normalized.get("orr")
    if not isinstance(orr_payload, dict):
        return normalized

    orr_payload["id"] = int(local_node_id)
    orr_payload["qkc_id"] = int(local_node_id)
    orr_host_payload = orr_payload.get("host")
    if runtime_service_name and isinstance(orr_host_payload, dict):
        orr_host_payload["ip"] = runtime_service_name

    qkc_payload = orr_payload.get("qkc")
    if not isinstance(qkc_payload, dict):
        return normalized

    qkc_payload["id"] = int(local_node_id)
    qkc_host_payload = qkc_payload.get("host")
    if runtime_service_name and isinstance(qkc_host_payload, dict):
        qkc_host_payload["ip"] = runtime_service_name

    raw_kmes = qkc_payload.get("kmes")
    if not isinstance(raw_kmes, list):
        return normalized

    for item in raw_kmes:
        if not isinstance(item, dict):
            continue

        neighbor_ip = item.get("neighbor_qkc_ip")
        neighbor_node_id = _infer_node_id_from_ip(neighbor_ip)
        if neighbor_node_id is None:
            neighbor_node_id = _safe_int(item.get("neighbor_qkc_id"))
        if neighbor_node_id is None or neighbor_node_id <= 0:
            continue

        # v3.3.1 fix: preserve the ORIGINAL qkc_ids (model-assigned, e.g.
        # 100001 for web-API sims) BEFORE normalizing item to node_ids.
        # PodQudittoLink names use the original qkc_ids, so the URL must
        # match. Also: use the SHORT service name (no .ns.svc.cluster.local
        # suffix) since DKMS pods live in the same namespace as the
        # quditto-link pods — avoids needing K8S_SIM_NAMESPACE env var.
        orig_local_qkc_id = _safe_int(item.get("local_qkc_id")) or int(local_node_id)
        orig_neighbor_qkc_id = _safe_int(item.get("neighbor_qkc_id")) or int(neighbor_node_id)

        item["local_qkc_id"] = int(local_node_id)
        item["neighbor_qkc_id"] = int(neighbor_node_id)
        # QuDitto config.yaml uses `DKMS-{qkc_id}` as node_name. The DKMS
        # client must ask for the SAME id (otherwise quditto returns 404).
        # We use the ORIGINAL qkc_id here, which matches what
        # PodQudittoLink writes into its configmap.
        item["local_qkd_id"] = f"DKMS-{orig_local_qkc_id}"
        item["neighbor_qkd_id"] = f"DKMS-{orig_neighbor_qkc_id}"
        item["etsi"] = "ETSI_014"
        link_a, link_b = sorted([orig_local_qkc_id, orig_neighbor_qkc_id])
        item["local_url_node_qkd"] = (
            f"http://quditto-link-{link_a}-{link_b}:5000"
        )
        if runtime_service_name:
            item["local_qkc_ip"] = runtime_service_name

        remote_host_id = _resolve_neighbor_qkc_service_suffix(
            local_host_id=local_host_id,
            local_node_id=int(local_node_id),
            neighbor_node_id=int(neighbor_node_id),
            raw_neighbor_qkc_ip=neighbor_ip,
        )
        if remote_host_id is not None:
            item["neighbor_qkc_ip"] = f"dkms-{remote_host_id}"

        cert_payload = item.get("cert")
        if not isinstance(cert_payload, dict):
            cert_payload = {}
            item["cert"] = cert_payload
        cert_payload["path"] = K8S_QUDITTO_CLIENT_CERT_PATH

        key_payload = item.get("key")
        if not isinstance(key_payload, dict):
            key_payload = {}
            item["key"] = key_payload
        key_payload["path"] = K8S_QUDITTO_CLIENT_KEY_PATH

    return normalized

class Pod:
    """
    Clase para gestionar la creación de recursos relacionados con DKMS en Kubernetes.
    Permite crear ConfigMaps, Volúmenes, Pods y Servicios de manera organizada.
    """

    __TYPE__ = None


    def __init__(self, id_simulation: str, model: Model):
        """
        Inicializa la clase con el nombre del recurso y el namespace.
        Carga la configuración de Kubernetes si es necesario.
        """
        if not self.__TYPE__:
            raise ValueError("Pod.__TYPE__ debe definirse en las subclases.")

        self.model = model
        name_suffix = getattr(model, "id_host", None)
        if name_suffix is None:
            name_suffix = getattr(model, "id", None)
        if name_suffix is None:
            raise ValueError("El modelo no tiene id_host ni id para construir el nombre.")

        # DNS-1035 (Service) exige empezar por letra; prefixamos el id para compatibilidad.
        self.name_suffix = str(name_suffix)
        self.name = f"{self.__TYPE__}-{self.name_suffix}"
        self.namespace = str(id_simulation)

        try:
            config.load_incluster_config()
        except ConfigException:
            config.load_kube_config()
        self.core_v1_api = client.CoreV1Api()
        self.apps_v1_api = client.AppsV1Api()
        self.networking_v1_api = client.NetworkingV1Api()
        self.containers = []
        self._config_map_items: Optional[list[client.V1KeyToPath]] = None

    def _read_namespace(self):
        try:
            return self.core_v1_api.read_namespace(name=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise

    def _read_namespaced(self, read_fn, name: str):
        try:
            return read_fn(name=name, namespace=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise

    def _delete_namespaced(self, delete_fn, name: str):
        try:
            return delete_fn(name=name, namespace=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise

    def _workload_labels(self) -> Dict[str, str]:
        return {
            "app": self.__TYPE__,
            "instance": self.name,
        }

    def _pod_template_annotations(self) -> Optional[Dict[str, str]]:
        if not K8S_PROTECT_SIM_PODS:
            return None
        if not K8S_DO_NOT_DISRUPT_ANNOTATION_KEY:
            return None
        return {K8S_DO_NOT_DISRUPT_ANNOTATION_KEY: "true"}

    def _build_deployment(
        self,
        containers: list[client.V1Container],
        volumes: Optional[list[client.V1Volume]] = None,
        image_pull_secret: str | None = None,
        init_containers: Optional[list[client.V1Container]] = None,
    ) -> client.V1Deployment:
        labels = self._workload_labels()
        pod_spec = client.V1PodSpec(
            containers=containers,
            init_containers=init_containers,
            volumes=volumes,
            restart_policy="Always",
            image_pull_secrets=(
                [client.V1LocalObjectReference(name=image_pull_secret)]
                if image_pull_secret
                else None
            ),
        )
        pod_template = client.V1PodTemplateSpec(
            metadata=client.V1ObjectMeta(
                labels=labels,
                annotations=self._pod_template_annotations(),
            ),
            spec=pod_spec,
        )
        return client.V1Deployment(
            metadata=client.V1ObjectMeta(name=self.name, labels=labels),
            spec=client.V1DeploymentSpec(
                replicas=1,
                selector=client.V1LabelSelector(match_labels=labels),
                template=pod_template,
            ),
        )

    def _config_map_enabled(self) -> bool:
        return K8S_CONFIG_MODE == "configmap"

    def _config_files_dir(self) -> Path:
        if K8S_CONFIG_FILES_DIR:
            return Path(K8S_CONFIG_FILES_DIR)
        return Path(__file__).resolve().parents[2] / "config_files"

    def _config_json_exists(self, section: str, identifier: int | None) -> bool:
        if identifier is None or identifier <= 0:
            return False
        return (self._config_files_dir() / section / f"{int(identifier)}.json").is_file()

    def _prefer_existing_config_id(self, section: str, candidates: list[int | None]) -> int | None:
        normalized: list[int] = []
        for raw in candidates:
            if raw is None:
                continue
            value = _safe_int(raw)
            if value is None or value <= 0:
                continue
            if value not in normalized:
                normalized.append(value)
        for value in normalized:
            if self._config_json_exists(section, value):
                return value
        if normalized:
            return normalized[0]
        return None

    def _infer_dkms_node_id(self) -> int | None:
        model = getattr(self, "model", None)
        host = getattr(model, "host", None)
        if host is None:
            return None

        ip_candidate = _infer_node_id_from_ip(getattr(host, "ip", None))
        if ip_candidate is not None:
            return ip_candidate

        port_candidate = _safe_int(getattr(host, "port", None))
        if port_candidate is not None and port_candidate > 4000:
            inferred = port_candidate - 4000
            if inferred > 0:
                return inferred
        return None

    def _resolve_sdn_config_id(self) -> int | None:
        model = getattr(self, "model", None)
        model_id = _safe_int(getattr(model, "id", None))
        return self._prefer_existing_config_id("SDN", [model_id, 1])

    def _resolve_dkms_config_id(self) -> int | None:
        model = getattr(self, "model", None)
        model_id = _safe_int(getattr(model, "id", None))
        inferred = self._infer_dkms_node_id()
        return self._prefer_existing_config_id("DKMS", [inferred, model_id])

    def _runtime_dkms_config_payload_b64(self) -> str | None:
        if self.__TYPE__ != "dkms":
            return None
        model = getattr(self, "model", None)
        model_dump = getattr(model, "model_dump", None)
        if not callable(model_dump):
            return None
        try:
            payload = model_dump(mode="json", exclude_none=True)
            payload = _canonicalize_runtime_dkms_payload(payload)
            raw_json = json.dumps(payload, ensure_ascii=True, separators=(",", ":"))
        except Exception:
            return None
        return base64.b64encode(raw_json.encode("utf-8")).decode("ascii")

    def _populate_dkms_config_env(self, env_payload: Dict[str, str]) -> None:
        dkms_cfg_id = self._resolve_dkms_config_id()
        if dkms_cfg_id is not None:
            # Compat: el SDN suele identificar nodos por IDs canónicos de config_files.
            env_payload["DKMS_SDN_NODE_ID"] = str(dkms_cfg_id)

        runtime_payload_b64 = self._runtime_dkms_config_payload_b64()
        if runtime_payload_b64:
            env_payload["DKMS_CONFIG"] = f"DKMS/runtime-{self.name_suffix}.json"
            env_payload["DKMS_CONFIG_JSON_B64"] = runtime_payload_b64
            return

        if dkms_cfg_id is not None:
            env_payload["DKMS_CONFIG"] = f"DKMS/{dkms_cfg_id}.json"

    def _config_map_payload(self) -> tuple[Dict[str, str], list[client.V1KeyToPath]]:
        config_dir = self._config_files_dir()
        if not config_dir.exists():
            raise FileNotFoundError(f"No existe el directorio de config_files: {config_dir}")
        data: Dict[str, str] = {}
        items: list[client.V1KeyToPath] = []
        for file_path in sorted(config_dir.rglob("*.json")):
            rel_path = file_path.relative_to(config_dir).as_posix()
            key = rel_path.replace("/", "__")
            if key in data:
                raise ValueError(f"Clave duplicada en ConfigMap: {key}")
            data[key] = file_path.read_text(encoding="utf-8")
            items.append(client.V1KeyToPath(key=key, path=rel_path))
        return data, items

    def _ingress_enabled(self) -> bool:
        return K8S_INGRESS_ENABLED

    def _service_port(self, model: Optional[Model] = None) -> int:
        if model is None:
            model = self.model
        host = getattr(model, "host", None)
        if host is None or getattr(host, "port", None) is None:
            raise ValueError("El modelo debe incluir host.port.")
        return host.port

    def _observability_enabled(self) -> bool:
        return K8S_OBSERVABILITY_ENABLED

    def _logs_volume_name(self) -> str:
        return "app-logs"

    def _promtail_config_volume_name(self) -> str:
        return "promtail-config"

    def _promtail_config_map_name(self) -> str:
        return "sim-promtail-config"

    def _promtail_container(
        self,
        image_pull_policy: str,
    ) -> client.V1Container:
        return client.V1Container(
            name="promtail",
            image=K8S_OBS_PROMTAIL_IMAGE,
            image_pull_policy=image_pull_policy,
            args=[
                "-config.file=/etc/promtail/promtail.yaml",
                "-config.expand-env=true",
            ],
            env=[
                client.V1EnvVar(
                    name="NAMESPACE",
                    value_from=client.V1EnvVarSource(
                        field_ref=client.V1ObjectFieldSelector(field_path="metadata.namespace")
                    ),
                ),
                client.V1EnvVar(
                    name="POD_NAME",
                    value_from=client.V1EnvVarSource(
                        field_ref=client.V1ObjectFieldSelector(field_path="metadata.name")
                    ),
                ),
            ],
            resources=client.V1ResourceRequirements(
                requests={"cpu": "20m", "memory": "64Mi"},
                limits={"cpu": "300m", "memory": "384Mi"},
            ),
            volume_mounts=[
                client.V1VolumeMount(
                    name=self._logs_volume_name(),
                    mount_path="/var/log/app",
                    read_only=True,
                ),
                client.V1VolumeMount(
                    name=self._promtail_config_volume_name(),
                    mount_path="/etc/promtail",
                    read_only=True,
                ),
            ],
        )

    def _ingress_templates_dir(self) -> Path:
        if K8S_INGRESS_TEMPLATES_DIR:
            return Path(K8S_INGRESS_TEMPLATES_DIR)
        return Path(__file__).resolve().parent / "ingress"

    def _render_ingress_template(self, template_name: str, replacements: Dict[str, str | int]):
        template_dir = self._ingress_templates_dir()
        template_path = template_dir / template_name
        if not template_path.exists():
            raise FileNotFoundError(f"No existe el template de ingress: {template_path}")
        content = template_path.read_text(encoding="utf-8")
        for key, value in replacements.items():
            content = content.replace(f"<{key}>", str(value))
        # Lazy import to avoid hard dependency when ingress is disabled.
        import yaml

        return yaml.safe_load(content)

    def _apply_ingress(self, body: dict):
        metadata = body.get("metadata") or {}
        name = metadata.get("name")
        if not name:
            raise ValueError("El template de ingress no define metadata.name")
        metadata["namespace"] = self.namespace
        body["metadata"] = metadata
        try:
            return self.networking_v1_api.create_namespaced_ingress(
                namespace=self.namespace, body=body
            )
        except ApiException as e:
            if e.status == 409:
                return self.networking_v1_api.replace_namespaced_ingress(
                    name=name, namespace=self.namespace, body=body
                )
            if self._is_missing_ingress_admission_service(e):
                raise RuntimeError(
                    "No se pudo crear el Ingress: falta el servicio "
                    "'ingress-nginx-controller-admission' en el namespace "
                    "'ingress-nginx' (webhook validate.nginx.ingress.kubernetes.io)."
                ) from e
            raise

    def _api_exception_message(self, exc: ApiException) -> str:
        body = getattr(exc, "body", "") or ""
        if not body:
            return str(exc)
        try:
            payload = json.loads(body)
        except (TypeError, json.JSONDecodeError):
            return body
        if not isinstance(payload, dict):
            return body

        message = payload.get("message")
        if isinstance(message, str):
            return message
        return body

    def _is_missing_ingress_admission_service(self, exc: ApiException) -> bool:
        if exc.status != 500:
            return False
        message = self._api_exception_message(exc).lower()
        return (
            "validate.nginx.ingress.kubernetes.io" in message
            and 'service "ingress-nginx-controller-admission" not found' in message
        )

    def _ensure_image_pull_secret(self) -> Optional[client.V1Secret]:
        if not DOCKER_HUB_TOKEN:
            return None
        if not DOCKER_HUB_USERNAME:
            raise ValueError("DOCKER_HUB_USERNAME debe definirse si DOCKER_HUB_TOKEN esta presente")

        existing = self._read_namespaced(
            self.core_v1_api.read_namespaced_secret, DOCKER_HUB_SECRET_NAME
        )
        if existing:
            return existing

        auth = base64.b64encode(
            f"{DOCKER_HUB_USERNAME}:{DOCKER_HUB_TOKEN}".encode("utf-8")
        ).decode("utf-8")
        dockerconfig = {
            "auths": {
                DOCKER_HUB_SERVER: {
                    "username": DOCKER_HUB_USERNAME,
                    "password": DOCKER_HUB_TOKEN,
                    "email": DOCKER_HUB_EMAIL or "",
                    "auth": auth,
                }
            }
        }

        secret = client.V1Secret(
            metadata=client.V1ObjectMeta(name=DOCKER_HUB_SECRET_NAME),
            type="kubernetes.io/dockerconfigjson",
            string_data={".dockerconfigjson": json.dumps(dockerconfig)},
        )
        try:
            return self.core_v1_api.create_namespaced_secret(
                namespace=self.namespace, body=secret
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(
                    self.core_v1_api.read_namespaced_secret, DOCKER_HUB_SECRET_NAME
                )
            raise
    
    def create_container(
        self,
        image: str,
        env_defaults: list | None = None,
        container_ports: list | None = None,
        extra_volume_mounts: list[client.V1VolumeMount] | None = None,
        image_pull_policy: str | None = None,
    ):
        if not image_pull_policy:
            image_pull_policy = K8S_IMAGE_PULL_POLICY
        volume_mount = self.crear_volume_mount()
        volume_mounts: list[client.V1VolumeMount] = []
        if volume_mount is not None:
            volume_mounts.append(volume_mount)
        if extra_volume_mounts:
            volume_mounts.extend(extra_volume_mounts)
        # Resources por tipo — override para componentes que son
        # single-pod y sufren contention (SDN en primer lugar).
        if self.__TYPE__ == "sdn":
            # v2.4.4: SDN OOMKilled con 4Gi en cluster 50 DKMSs bajo carga
            # de PATCHs frecuentes. El solver MCF lex-max-min con N=50
            # commodities mantiene snapshots y push queues. 8Gi cubre el
            # peak con margen.
            resource_limits = {
                "cpu": os.getenv("K8S_SDN_CPU_LIMIT", "4000m"),
                "memory": os.getenv("K8S_SDN_MEMORY_LIMIT", "8Gi"),
            }
            resource_requests = {
                "cpu": os.getenv("K8S_SDN_CPU_REQUEST", "1000m"),
                "memory": os.getenv("K8S_SDN_MEMORY_REQUEST", "2Gi"),
            }
        else:
            resource_limits = {
                "cpu": os.getenv("K8S_DEFAULT_CPU_LIMIT", "1000m"),
                "memory": os.getenv("K8S_DEFAULT_MEMORY_LIMIT", "1Gi"),
            }
            resource_requests = {
                "cpu": os.getenv("K8S_DEFAULT_CPU_REQUEST", "200m"),
                "memory": os.getenv("K8S_DEFAULT_MEMORY_REQUEST", "256Mi"),
            }
        container = client.V1Container(
            name=self.__TYPE__,
            image=image,
            image_pull_policy=image_pull_policy,
            env=env_defaults,
            resources=client.V1ResourceRequirements(
                requests=resource_requests,
                limits=resource_limits,
            ),
            ports=container_ports,
            volume_mounts=volume_mounts if volume_mounts else None,
        )
        self.containers.append(container)

    def create_namespace(self):
        existing = self._read_namespace()
        if existing:
            self._ensure_image_pull_secret()
            return existing

        namespace = client.V1Namespace(
            metadata=client.V1ObjectMeta(
                name=self.namespace
            )
        )
        try:
            created = self.core_v1_api.create_namespace(body=namespace)
            self._ensure_image_pull_secret()
            return created
        except ApiException as e:
            if e.status == 409:
                existing = self._read_namespace()
                if existing:
                    self._ensure_image_pull_secret()
                return existing
            raise

    def eliminar_namespace(self):
        """
        Elimina el namespace configurado si existe.
        """
        try:
            return self.core_v1_api.delete_namespace(name=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise


    def crear_config_map(self):
        """
        Crea un ConfigMap de Kubernetes con el nombre del recurso en el namespace especificado.
        """
        if not self._config_map_enabled():
            return None

        existing = self._read_namespaced(self.core_v1_api.read_namespaced_config_map, self.name)
        if existing:
            return existing

        data, items = self._config_map_payload()
        self._config_map_items = items
        config_map = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(name=self.name),
            data=data,
        )
        try:
            return self.core_v1_api.create_namespaced_config_map(
                namespace=self.namespace,
                body=config_map
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.core_v1_api.read_namespaced_config_map, self.name)
            raise

    def eliminar_config_map(self):
        """
        Elimina el ConfigMap del DKMS si existe.
        """
        return self._delete_namespaced(self.core_v1_api.delete_namespaced_config_map, self.name)

    def crear_volume(self):
        """
        Crea un volumen que monta el ConfigMap creado anteriormente.
        """
        if not self._config_map_enabled():
            return None
        return client.V1Volume(
            name="config",
            config_map=client.V1ConfigMapVolumeSource(
                name=self.name,
                items=self._config_map_items,
            )
        )

    def crear_volume_mount(self):
        """
        Devuelve la especificación de montaje del volumen en el contenedor.
        """
        if not self._config_map_enabled():
            return None
        return client.V1VolumeMount(
            name="config",
            mount_path="/app/config",
            read_only=True
        )

    def create_pod(
        self,
        env_vars: dict | None = None,
        image: str | None = None,
        ports: list | None = None,
        image_pull_secret: str | None = None,
        image_pull_policy: str | None = None,
    ):
        """
        Crea un Deployment DKMS/SDN en el namespace configurado.
        Permite especificar variables de entorno y puertos.
        """
        existing = self._read_namespaced(self.apps_v1_api.read_namespaced_deployment, self.name)
        if existing:
            return existing

        if image is None:
            image = f"{self.__TYPE__}:latest"
        if not image_pull_policy:
            image_pull_policy = K8S_IMAGE_PULL_POLICY

        host_port = None
        host = getattr(self.model, "host", None)
        if host is not None:
            host_port = getattr(host, "port", None)

        env_payload: Dict[str, str] = {
            "CONFIG_FOLDER": "/app/config",
            "LOG_DIR": "/app/logs",
        }
        if self.__TYPE__ == "sdn":
            env_payload["SDN_BIND_IP"] = "0.0.0.0"
            if host_port is not None:
                env_payload["SDN_PORT"] = str(host_port)
            sdn_cfg_id = self._resolve_sdn_config_id()
            if sdn_cfg_id is not None:
                env_payload["SDN_CONFIG"] = f"SDN/{sdn_cfg_id}.json"
            # Rama sp-rr-saturated: el solver SP+RR está hardcoded en
            # mcf.py.solve(). No hace falta inyectar SDN_MCF_USE_* vars
            # — quedan reservadas para diagnóstico manual o experiments.
            # scipy.optimize.linprog(method="highs") + numpy invocan a
            # OpenBLAS/libgomp/MKL en cada solve. Sin techo explícito,
            # spawnean cientos de OS-threads (medido: 432 threads, +15/5s
            # bajo cluster N=50). Cada uno reserva ~8 MB de stack: el OOM
            # con limit 8 GB se explica casi entero por el stack agregado.
            # Con valores a 1 el solver no pierde rendimiento real (el
            # LP es pequeño y los workers múltiples sólo añaden contention)
            # y los threads bajan a régimen estable.
            env_payload.setdefault("OPENBLAS_NUM_THREADS", "1")
            env_payload.setdefault("OMP_NUM_THREADS", "1")
            env_payload.setdefault("MKL_NUM_THREADS", "1")
            env_payload.setdefault("NUMEXPR_NUM_THREADS", "1")
            # glibc malloc: cada solve aloca matrices ~160 MB que numpy
            # libera enseguida, pero glibc las retiene en arenas (una
            # por thread → hasta 8×CPU por defecto). Con la SDN
            # recompilando cada N segundos eso fragmenta el heap y el
            # cgroup ve crecimiento monótono de RSS hasta OOMKill.
            #   ARENA_MAX=2 limita las arenas a 2 (en vez de 8×CPU).
            #   TRIM_THRESHOLD_=131072 fuerza al malloc a hacer
            #     madvise(DONTNEED) sobre 128 KB+ libres contiguos.
            env_payload.setdefault("MALLOC_ARENA_MAX", "2")
            env_payload.setdefault("MALLOC_TRIM_THRESHOLD_", "131072")
        elif self.__TYPE__ == "dkms":
            env_payload["BIND_IP"] = "0.0.0.0"
            env_payload["AGENT_CONTROLLER_PORT"] = "8080"
            env_payload["DKMS_ADVERTISED_HOST"] = self.name
            # v2.10.8 final: KME_LINK_STORE_ENABLED=false por default.
            # Iter 8/9 históricos rolledback con cap LRU 5000.
            # Iter 10 (esta sesión) probó con cap 20000 + drain 100ms.
            # Resultado: drain `all_keys=true` cada 100ms satura CPU
            # tanto en quditto (serialización JSON masiva) como en KME
            # parent (parsing+extend), regresión 980/s vs 2200/s del
            # prefetch básico. Manteniendo OFF.
            # Patch F12: parallel decrypt activo. link_store y drain
            # on-demand DESACTIVADOS por default tras medición F12:
            # provocaron regresión de throughput (latencia 7s/req).
            env_payload.setdefault("KME_LINK_STORE_ENABLED", "false")
            env_payload.setdefault("KME_LINK_STORE_NO_PERIODIC", "1")
            env_payload.setdefault("QKC_RECV_PARALLEL", "0")
            env_payload.setdefault("QKC_RECV_PARALLELISM", "64")
            env_payload.setdefault("QKC_DEC_DRAIN_ON_MISS", "0")
            env_payload.setdefault("QKC_DEC_DRAIN_ON_MISS_INTERVAL_MS", "20")
            # F26: max_tokens=512 + batch=64 (con cap=10000 sin oscilación).
            env_payload.setdefault("DKMS_GEN_MAX_TOKENS_PER_PEER_PER_TICK", "512")
            env_payload.setdefault("DKMS_GENERATOR_BATCH_SIZE", "64")
            # F14: QKC_ENCRYPT_PROCESS_WORKERS=0 (de nuevo). El subprocess
            # path NO comparte la `_enc_prefetch_queue` con el parent. Cada
            # call hace un HTTP fresco a quditto. Bajo conc=64 + size=6936
            # (single-key inflation) quditto cae a 72 calls/s/sidecar.
            # Path in-thread con prefetch llena la cola en background
            # (1 thread per KME, num=64 keys/HTTP) → 12k keys/s/sidecar.
            env_payload.setdefault("QKC_ENCRYPT_PROCESS_WORKERS", "0")
            # F14: single-key inflation OFF. Si está activa (8192 default)
            # los send_workers piden size=6936 (1 key gigante por payload)
            # que bypassea el prefetch (`_pop_prefetched_enc_key` retorna
            # None si size_bits != 256). Con 0 el encrypt chunkea por
            # 256 bits → cada chunk popea del prefetch en O(1).
            env_payload.setdefault("QKC_ENCRYPT_SINGLE_KEY_MAX_BITS", "0")
            self._populate_dkms_config_env(env_payload)
        if env_vars:
            for key, value in env_vars.items():
                env_payload[str(key)] = str(value)
        env_defaults = [client.V1EnvVar(name=key, value=value) for key, value in env_payload.items()]
        container_port = host_port or 8080
        container_ports = [client.V1ContainerPort(container_port=container_port)]
        if self.__TYPE__ == "dkms" and container_port != 8080:
            container_ports.append(client.V1ContainerPort(container_port=8080))
        if ports:
            for p in ports:
                container_ports.append(client.V1ContainerPort(container_port=p))


        if image_pull_secret is None and DOCKER_HUB_TOKEN:
            image_pull_secret = DOCKER_HUB_SECRET_NAME
            self._ensure_image_pull_secret()

        self.containers = []
        extra_mounts = [
            client.V1VolumeMount(
                name=self._logs_volume_name(),
                mount_path="/app/logs",
                read_only=False,
            )
        ]
        self.create_container(
            image=image,
            env_defaults=env_defaults,
            container_ports=container_ports,
            extra_volume_mounts=extra_mounts,
            image_pull_policy=image_pull_policy,
        )

        if self._observability_enabled():
            self.containers.append(self._promtail_container(image_pull_policy=image_pull_policy))

        volume = self.crear_volume()
        volumes: list[client.V1Volume] = [volume] if volume else []
        volumes.append(
            client.V1Volume(
                name=self._logs_volume_name(),
                empty_dir=client.V1EmptyDirVolumeSource(),
            )
        )
        if self._observability_enabled():
            volumes.append(
                client.V1Volume(
                    name=self._promtail_config_volume_name(),
                    config_map=client.V1ConfigMapVolumeSource(
                        name=self._promtail_config_map_name()
                    ),
                )
            )

        deployment = self._build_deployment(
            containers=self.containers,
            volumes=volumes if volumes else None,
            image_pull_secret=image_pull_secret,
        )
        try:
            return self.apps_v1_api.create_namespaced_deployment(
                namespace=self.namespace,
                body=deployment
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.apps_v1_api.read_namespaced_deployment, self.name)
            raise

    def eliminar_pod(self):
        """
        Elimina el Deployment del DKMS/SDN si existe.
        """
        return self._delete_namespaced(self.apps_v1_api.delete_namespaced_deployment, self.name)

    def create_service(self):
        """
        Crea un servicio ClusterIP para el pod DKMS.
        """
        existing = self._read_namespaced(self.core_v1_api.read_namespaced_service, self.name)
        if existing:
            return existing

        port = self._service_port()

        service = client.V1Service(
            metadata=client.V1ObjectMeta(
                name=self.name
            ),
            spec=client.V1ServiceSpec(
                selector=self._workload_labels(),
                ports=[
                    client.V1ServicePort(
                        name="app",
                        port=port,
                        target_port=port
                    )
                ],
                type="ClusterIP"
            )
        )
        try:
            return self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=service
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.core_v1_api.read_namespaced_service, self.name)
            raise

    def _ensure_external_name_service(
        self,
        *,
        service_name: str,
        external_name: str,
        port: int,
    ):
        existing = self._read_namespaced(self.core_v1_api.read_namespaced_service, service_name)
        if existing is not None:
            try:
                existing_type = str(getattr(getattr(existing, "spec", None), "type", "") or "")
                existing_external = str(getattr(getattr(existing, "spec", None), "external_name", "") or "")
            except Exception:  # noqa: BLE001
                existing_type = ""
                existing_external = ""
            existing_ports = getattr(getattr(existing, "spec", None), "ports", None) or []
            existing_port = None
            if existing_ports:
                try:
                    existing_port = int(getattr(existing_ports[0], "port", None))
                except (TypeError, ValueError):
                    existing_port = None

            if (
                existing_type.lower() == "externalname"
                and existing_external == external_name
                and existing_port == int(port)
            ):
                return existing

            patch_body = {
                "spec": {
                    "type": "ExternalName",
                    "externalName": external_name,
                    "ports": [
                        {"name": "http", "port": int(port), "targetPort": int(port)}
                    ],
                }
            }
            return self.core_v1_api.patch_namespaced_service(
                name=service_name,
                namespace=self.namespace,
                body=patch_body,
            )

        service = client.V1Service(
            metadata=client.V1ObjectMeta(name=service_name),
            spec=client.V1ServiceSpec(
                type="ExternalName",
                external_name=external_name,
                ports=[
                    client.V1ServicePort(
                        name="http",
                        port=int(port),
                        target_port=int(port),
                    )
                ],
            ),
        )
        try:
            return self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=service,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.core_v1_api.read_namespaced_service, service_name)
            raise

    def create_ingress(self):
        """
        Crea un Ingress para el recurso (DKMS o SDN) a partir de un template YAML.
        """
        if not self._ingress_enabled():
            return None

        port = self._service_port()

        replacements = {
            "SIM_NAMESPACE": self.namespace,
            "SIM_ID": self.namespace,
            "INGRESS_HOST": K8S_INGRESS_HOST,
            "INGRESS_CLASS": K8S_INGRESS_CLASS,
            "INGRESS_AUTH_URL": K8S_INGRESS_AUTH_URL,
            "INGRESS_AUTH_HEADERS": K8S_INGRESS_AUTH_HEADERS,
        }

        if self.__TYPE__ == "dkms":
            replacements.update(
                {
                    "DKMS_ID": self.name_suffix,
                    "DKMS_PORT": port,
                }
            )
            body = self._render_ingress_template("ingress-dkms.yaml", replacements)
        elif self.__TYPE__ == "sdn":
            replacements.update(
                {
                    "SDN_ID": self.name_suffix,
                    "SDN_PORT": port,
                }
            )
            body = self._render_ingress_template("ingress-sdn.yaml", replacements)
        else:
            return None

        return self._apply_ingress(body)

    def create_simulation_ingress(
        self,
        dkms_pods: list["PodDKMS"],
        sdn_pod: "PodSDN",
    ):
        """
        Crea un Ingress unico por simulacion con rutas a DKMS y SDN.
        """
        if not self._ingress_enabled():
            return None

        if sdn_pod is None:
            raise ValueError("Se requiere el pod SDN para crear el Ingress de simulacion.")

        sim_id = self.namespace

        auth_annotations = {
            "nginx.ingress.kubernetes.io/auth-url": K8S_INGRESS_AUTH_URL,
            "nginx.ingress.kubernetes.io/auth-response-headers": K8S_INGRESS_AUTH_HEADERS,
            "nginx.ingress.kubernetes.io/auth-snippet": (
                "proxy_set_header Authorization $http_authorization;\n"
                "proxy_set_header Cookie $http_cookie;"
            ),
            "nginx.ingress.kubernetes.io/use-regex": "true",
        }

        main_annotations = dict(auth_annotations)
        main_annotations["nginx.ingress.kubernetes.io/rewrite-target"] = "/$2"

        def _build_path(path: str, service_name: str, service_port: int) -> dict:
            return {
                "path": path,
                "pathType": "ImplementationSpecific",
                "backend": {
                    "service": {
                        "name": service_name,
                        "port": {"number": service_port},
                    }
                },
            }

        gateway_service_name = "orchestator-gateway"
        gateway_service_port = 8080
        ensure_external_name = getattr(self, "_ensure_external_name_service", None)
        if callable(ensure_external_name):
            ensure_external_name(
                service_name=gateway_service_name,
                external_name="orchestator.dkms-main-ns.svc.cluster.local",
                port=gateway_service_port,
            )

        ingress_host = (K8S_INGRESS_HOST or "").strip()
        runtime_ingress_host = (K8S_RUNTIME_INGRESS_HOST or ingress_host).strip()

        sdn_sae_annotations = dict(auth_annotations)
        sdn_sae_annotations["nginx.ingress.kubernetes.io/rewrite-target"] = (
            f"/orch/api/sim/{sim_id}/sdn/$1"
        )
        sdn_sae_paths: list[dict] = [
            _build_path(
                f"/api/sim/{sim_id}/sdn/(sae(/|$).*)",
                gateway_service_name,
                gateway_service_port,
            ),
            _build_path(
                f"/api/sim/{sim_id}/sdn/(resolve-sae)",
                gateway_service_name,
                gateway_service_port,
            ),
        ]
        sdn_sae_rule: dict = {"http": {"paths": sdn_sae_paths}}
        if ingress_host and ingress_host != "*":
            sdn_sae_rule["host"] = ingress_host
        sdn_sae_ingress_body = {
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": {
                "name": f"sim-{sim_id}-sdn-sae-ingress",
                "annotations": sdn_sae_annotations,
            },
            "spec": {
                "ingressClassName": K8S_INGRESS_CLASS,
                "rules": [sdn_sae_rule],
            },
        }
        self._apply_ingress(sdn_sae_ingress_body)

        sdn_paths: list[dict] = []
        dkms_paths: list[dict] = []

        sdn_port = self._service_port(sdn_pod.model)
        sdn_paths.append(
            _build_path(
                f"/api/sim/{sim_id}/sdn(/|$)(.*)",
                sdn_pod.name,
                sdn_port,
            )
        )

        for dkms_pod in dkms_pods:
            dkms_port = self._service_port(dkms_pod.model)
            dkms_paths.append(
                _build_path(
                    f"/api/sim/{sim_id}/dkms/{dkms_pod.name_suffix}(/|$)(.*)",
                    dkms_pod.name,
                    dkms_port,
                )
            )

        rule: dict = {"http": {"paths": sdn_paths}}
        if ingress_host and ingress_host != "*":
            rule["host"] = ingress_host

        body = {
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": {
                "name": f"sim-{sim_id}-ingress",
                "annotations": main_annotations,
            },
            "spec": {
                "ingressClassName": K8S_INGRESS_CLASS,
                "rules": [rule],
            },
        }
        main_ingress = self._apply_ingress(body)

        if dkms_paths:
            runtime_annotations: Dict[str, str] = {
                "nginx.ingress.kubernetes.io/use-regex": "true",
                "nginx.ingress.kubernetes.io/rewrite-target": "/$2",
            }
            read_namespaced = getattr(self, "_read_namespaced", None)
            core_v1_api = getattr(self, "core_v1_api", None)
            can_validate_runtime_mtls = (
                callable(read_namespaced)
                and core_v1_api is not None
                and hasattr(core_v1_api, "read_namespaced_secret")
            )
            if K8S_RUNTIME_MTLS_ENABLED and can_validate_runtime_mtls:
                runtime_ca_secret = self._read_namespaced(
                    self.core_v1_api.read_namespaced_secret,
                    K8S_RUNTIME_MTLS_CA_SECRET,
                )
                if runtime_ca_secret is None:
                    raise RuntimeError(
                        "Runtime mTLS habilitado pero falta el secret de CA "
                        f"{self.namespace}/{K8S_RUNTIME_MTLS_CA_SECRET}."
                    )
                runtime_ca_data = getattr(runtime_ca_secret, "data", None) or {}
                if not runtime_ca_data.get("ca.crt"):
                    raise RuntimeError(
                        "Runtime mTLS habilitado pero el secret de CA "
                        f"{self.namespace}/{K8S_RUNTIME_MTLS_CA_SECRET} no contiene ca.crt."
                    )
                runtime_annotations.update(
                    {
                        "nginx.ingress.kubernetes.io/auth-tls-verify-client": "on",
                        "nginx.ingress.kubernetes.io/auth-tls-secret": f"{self.namespace}/{K8S_RUNTIME_MTLS_CA_SECRET}",
                        "nginx.ingress.kubernetes.io/auth-tls-verify-depth": str(
                            K8S_RUNTIME_MTLS_VERIFY_DEPTH
                        ),
                        "nginx.ingress.kubernetes.io/auth-tls-pass-certificate-to-upstream": "true",
                    }
                )
            elif K8S_RUNTIME_MTLS_ENABLED:
                runtime_annotations.update(auth_annotations)
            else:
                runtime_annotations.update(auth_annotations)

            runtime_rule: dict = {"http": {"paths": dkms_paths}}
            if runtime_ingress_host and runtime_ingress_host != "*":
                runtime_rule["host"] = runtime_ingress_host

            runtime_ingress_body = {
                "apiVersion": "networking.k8s.io/v1",
                "kind": "Ingress",
                "metadata": {
                    "name": f"sim-{sim_id}-runtime-ingress",
                    "annotations": runtime_annotations,
                },
                "spec": {
                    "ingressClassName": K8S_INGRESS_CLASS,
                    "rules": [runtime_rule],
                },
            }
            self._apply_ingress(runtime_ingress_body)

        if not self._observability_enabled():
            return main_ingress

        grafana_rule: dict = {
            "http": {
                "paths": [
                    _build_path(
                        f"/api/sim/{sim_id}/grafana(/|$)(.*)",
                        "sim-grafana",
                        K8S_OBS_GRAFANA_PORT,
                    )
                ]
            }
        }
        if ingress_host and ingress_host != "*":
            grafana_rule["host"] = ingress_host

        grafana_annotations = {
            "nginx.ingress.kubernetes.io/use-regex": "true",
        }

        grafana_ingress_body = {
            "apiVersion": "networking.k8s.io/v1",
            "kind": "Ingress",
            "metadata": {
                "name": f"sim-{sim_id}-grafana-ingress",
                "annotations": grafana_annotations,
            },
            "spec": {
                "ingressClassName": K8S_INGRESS_CLASS,
                "rules": [grafana_rule],
            },
        }
        self._apply_ingress(grafana_ingress_body)
        return main_ingress

    def eliminar_servicio(self):
        """
        Elimina el servicio del DKMS si existe.
        """
        return self._delete_namespaced(self.core_v1_api.delete_namespaced_service, self.name)


class PodObservability:
    """Stack de observabilidad por simulación: Prometheus + Loki + Grafana."""

    def __init__(self, id_simulation: str):
        self.namespace = str(id_simulation)
        self.prometheus_name = "sim-prometheus"
        self.loki_name = "sim-loki"
        self.grafana_name = "sim-grafana"
        self.prometheus_pvc_name = "sim-prometheus-data"
        self.loki_pvc_name = "sim-loki-data"
        self.grafana_pvc_name = "sim-grafana-data"
        self.prometheus_config_name = "sim-prometheus-config"
        self.loki_config_name = "sim-loki-config"
        self.promtail_config_name = "sim-promtail-config"
        self.grafana_datasources_name = "sim-grafana-datasources"
        self.grafana_dashboards_provider_name = "sim-grafana-dashboards-provider"
        self.grafana_dashboards_name = "sim-grafana-dashboards"
        try:
            config.load_incluster_config()
        except ConfigException:
            config.load_kube_config()
        self.core_v1_api = client.CoreV1Api()
        self.apps_v1_api = client.AppsV1Api()

    def _pod_template_annotations(self) -> Optional[Dict[str, str]]:
        if not K8S_PROTECT_SIM_PODS:
            return None
        if not K8S_DO_NOT_DISRUPT_ANNOTATION_KEY:
            return None
        return {K8S_DO_NOT_DISRUPT_ANNOTATION_KEY: "true"}

    def _read_namespaced(self, read_fn, name: str):
        try:
            return read_fn(name=name, namespace=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise

    def _upsert_config_map(self, name: str, data: Dict[str, str]):
        body = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(name=name),
            data=data,
        )
        try:
            return self.core_v1_api.create_namespaced_config_map(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as e:
            if e.status != 409:
                raise
            existing = self._read_namespaced(
                self.core_v1_api.read_namespaced_config_map, name
            )
            if existing is not None and existing.metadata is not None:
                rv = getattr(existing.metadata, "resource_version", None)
                if body.metadata is None:
                    body.metadata = client.V1ObjectMeta(name=name)
                body.metadata.resource_version = rv
            return self.core_v1_api.replace_namespaced_config_map(
                name=name,
                namespace=self.namespace,
                body=body,
            )

    def _upsert_service(self, name: str, selector: Dict[str, str], port: int):
        existing = self._read_namespaced(self.core_v1_api.read_namespaced_service, name)
        if existing:
            return existing
        body = client.V1Service(
            metadata=client.V1ObjectMeta(name=name),
            spec=client.V1ServiceSpec(
                selector=selector,
                ports=[
                    client.V1ServicePort(
                        name="http",
                        port=port,
                        target_port=port,
                    )
                ],
                type="ClusterIP",
            ),
        )
        try:
            return self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.core_v1_api.read_namespaced_service, name)
            raise

    def _volume_requirements(self, size: str):
        if hasattr(client, "V1VolumeResourceRequirements"):
            return client.V1VolumeResourceRequirements(requests={"storage": size})
        return client.V1ResourceRequirements(requests={"storage": size})

    def _upsert_pvc(self, name: str, size: str):
        existing = self._read_namespaced(
            self.core_v1_api.read_namespaced_persistent_volume_claim, name
        )
        if existing:
            return existing
        body = client.V1PersistentVolumeClaim(
            metadata=client.V1ObjectMeta(name=name),
            spec=client.V1PersistentVolumeClaimSpec(
                access_modes=["ReadWriteOnce"],
                resources=self._volume_requirements(size),
                storage_class_name=(K8S_OBS_STORAGE_CLASS or None),
            ),
        )
        try:
            return self.core_v1_api.create_namespaced_persistent_volume_claim(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(
                    self.core_v1_api.read_namespaced_persistent_volume_claim, name
                )
            raise

    def _data_volume(self, name: str, pvc_name: str) -> client.V1Volume:
        if K8S_OBS_PERSISTENCE_ENABLED:
            return client.V1Volume(
                name=name,
                persistent_volume_claim=client.V1PersistentVolumeClaimVolumeSource(
                    claim_name=pvc_name
                ),
            )
        return client.V1Volume(name=name, empty_dir=client.V1EmptyDirVolumeSource())

    def _upsert_deployment(self, name: str, labels: Dict[str, str], pod_spec: client.V1PodSpec):
        body = client.V1Deployment(
            metadata=client.V1ObjectMeta(name=name, labels=labels),
            spec=client.V1DeploymentSpec(
                replicas=1,
                selector=client.V1LabelSelector(match_labels=labels),
                template=client.V1PodTemplateSpec(
                    metadata=client.V1ObjectMeta(
                        labels=labels,
                        annotations=self._pod_template_annotations(),
                    ),
                    spec=pod_spec,
                ),
            ),
        )
        try:
            return self.apps_v1_api.create_namespaced_deployment(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as e:
            if e.status != 409:
                raise
            last_exc: ApiException = e
            for _ in range(5):
                existing = self._read_namespaced(
                    self.apps_v1_api.read_namespaced_deployment, name
                )
                if existing is not None and existing.metadata is not None:
                    rv = getattr(existing.metadata, "resource_version", None)
                    if body.metadata is None:
                        body.metadata = client.V1ObjectMeta(name=name, labels=labels)
                    body.metadata.resource_version = rv
                try:
                    return self.apps_v1_api.replace_namespaced_deployment(
                        name=name,
                        namespace=self.namespace,
                        body=body,
                    )
                except ApiException as replace_exc:
                    if replace_exc.status != 409:
                        raise
                    last_exc = replace_exc
                    continue
            raise last_exc

    def _model_service_port(self, model: Model) -> int:
        host = getattr(model, "host", None)
        if host is None or getattr(host, "port", None) is None:
            raise ValueError("El modelo debe incluir host.port para observabilidad.")
        return int(host.port)

    def _prometheus_config(
        self,
        dkms_pods: list["PodDKMS"],
        sdn_pod: "PodSDN",
        link_pods: Optional[list["PodQudittoLink"]] = None,
    ) -> str:
        sdn_target = f"{sdn_pod.name}:{self._model_service_port(sdn_pod.model)}"
        dkms_targets = sorted(
            {
                f"{pod.name}:{self._model_service_port(pod.model)}"
                for pod in dkms_pods
            }
        )
        # v3.3: quditto-link pods independientes exponen métricas en el
        # mismo puerto 8000 que el sidecar viejo, pero en Service propio.
        quditto_targets = sorted(
            {
                f"{pod.name}:{K8S_QUDITTO_METRICS_PORT}"
                for pod in (link_pods or [])
            }
        )
        scrape_configs: list[dict[str, Any]] = [
            {
                "job_name": "sdn",
                "metrics_path": "/metrics",
                "static_configs": [
                    {
                        "targets": [sdn_target],
                        "labels": {
                            "simulation_id": self.namespace,
                            "component": "sdn",
                        },
                    }
                ],
            }
        ]
        if dkms_targets:
            scrape_configs.append(
                {
                    "job_name": "dkms",
                    "metrics_path": "/metrics",
                    "static_configs": [
                        {
                            "targets": dkms_targets,
                            "labels": {
                                "simulation_id": self.namespace,
                                "component": "dkms",
                            },
                        }
                    ],
                }
            )
        if quditto_targets:
            scrape_configs.append(
                {
                    "job_name": "quditto",
                    "metrics_path": "/metrics",
                    "static_configs": [
                        {
                            "targets": quditto_targets,
                            "labels": {
                                "simulation_id": self.namespace,
                                "component": "quditto",
                            },
                        }
                    ],
                }
            )
        # Load-test runners: cada test crea un Deployment con label
        # `app=loadtest-runner` y un Service headless `loadtest-runners`.
        # Prometheus descubre dinámicamente los pods vía DNS-SD. Cuando no
        # hay tests activos, el lookup devuelve NXDOMAIN y la sección
        # simplemente no scrapea — no hay error.
        scrape_configs.append(
            {
                "job_name": "loadtest",
                "metrics_path": "/metrics",
                "scrape_interval": "5s",
                "dns_sd_configs": [
                    {
                        "names": [f"loadtest-runners.{self.namespace}.svc.cluster.local"],
                        "type": "A",
                        "port": K8S_LOADTEST_METRICS_PORT,
                    }
                ],
                "relabel_configs": [
                    {
                        "source_labels": ["__meta_dns_name"],
                        "target_label": "service",
                    }
                ],
            }
        )
        payload = {
            "global": {
                "scrape_interval": "10s",
                "evaluation_interval": "10s",
            },
            "scrape_configs": scrape_configs,
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def _loki_config(self) -> str:
        payload = {
            "auth_enabled": False,
            "server": {"http_listen_port": K8S_OBS_LOKI_PORT},
            "common": {
                "path_prefix": "/loki",
                "storage": {
                    "filesystem": {
                        "chunks_directory": "/loki/chunks",
                        "rules_directory": "/loki/rules",
                    }
                },
                "replication_factor": 1,
                "ring": {"kvstore": {"store": "inmemory"}},
            },
            "schema_config": {
                "configs": [
                    {
                        "from": "2024-01-01",
                        "store": "tsdb",
                        "object_store": "filesystem",
                        "schema": "v13",
                        "index": {"prefix": "index_", "period": "24h"},
                    }
                ]
            },
            "limits_config": {"retention_period": K8S_OBS_LOKI_RETENTION},
            "compactor": {
                "working_directory": "/loki/compactor",
                "compaction_interval": "10m",
                "retention_enabled": True,
            },
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def _promtail_config(self) -> str:
        payload = {
            "server": {"http_listen_port": 9080},
            "positions": {"filename": "/tmp/positions.yaml"},
            "clients": [
                {
                    "url": (
                        f"http://{self.loki_name}:{K8S_OBS_LOKI_PORT}"
                        "/loki/api/v1/push"
                    )
                }
            ],
            "scrape_configs": [
                {
                    "job_name": "dkms",
                    "static_configs": [
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/DKMS/*.log",
                            },
                        },
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/ORR/*.log",
                            },
                        },
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/QKC/*.log",
                            },
                        },
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/KME/*.log",
                            },
                        },
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/token_bucket/*.log",
                            },
                        },
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "dkms",
                                "__path__": "/var/log/app/AgentController/*.log",
                            },
                        },
                    ],
                },
                {
                    "job_name": "quditto",
                    "static_configs": [
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "quditto",
                                "__path__": "/var/log/app/quditto/*.log",
                            },
                        }
                    ],
                },
                {
                    "job_name": "sdn",
                    "static_configs": [
                        {
                            "targets": ["localhost"],
                            "labels": {
                                "simulation_id": "${NAMESPACE}",
                                "namespace": "${NAMESPACE}",
                                "pod": "${POD_NAME}",
                                "component": "sdn",
                                "__path__": "/var/log/app/sdn.log",
                            },
                        }
                    ],
                },
            ],
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def _grafana_datasources(self) -> str:
        payload = {
            "apiVersion": 1,
            "datasources": [
                {
                    "name": "Prometheus",
                    "uid": K8S_OBS_GRAFANA_PROMETHEUS_UID,
                    "type": "prometheus",
                    "access": "proxy",
                    "url": (
                        f"http://{self.prometheus_name}:{K8S_OBS_PROMETHEUS_PORT}"
                    ),
                    "isDefault": True,
                },
                {
                    "name": "Loki",
                    "uid": K8S_OBS_GRAFANA_LOKI_UID,
                    "type": "loki",
                    "access": "proxy",
                    "url": f"http://{self.loki_name}:{K8S_OBS_LOKI_PORT}",
                },
            ],
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def _grafana_dashboard_json(self) -> str:
        dashboard_path = Path(__file__).resolve().with_name("grafana_buffers_dashboard.json")
        return dashboard_path.read_text(encoding="utf-8")

    def _grafana_loadtest_dashboard_json(self) -> str:
        dashboard_path = Path(__file__).resolve().with_name("grafana_loadtest_dashboard.json")
        return dashboard_path.read_text(encoding="utf-8")

    def _grafana_dashboards_provider(self) -> str:
        payload = {
            "apiVersion": 1,
            "providers": [
                {
                    "name": "default",
                    "orgId": 1,
                    "folder": "",
                    "type": "file",
                    "disableDeletion": False,
                    "editable": True,
                    "allowUiUpdates": True,
                    "options": {
                        "path": "/var/lib/grafana/dashboards",
                    },
                }
            ],
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def deploy(
        self,
        dkms_pods: list["PodDKMS"],
        sdn_pod: "PodSDN",
        image_pull_secret: str | None = None,
        image_pull_policy: str | None = None,
        link_pods: Optional[list["PodQudittoLink"]] = None,
    ):
        if image_pull_policy is None:
            image_pull_policy = K8S_IMAGE_PULL_POLICY

        self._upsert_config_map(
            self.prometheus_config_name,
            {
                "prometheus.yml": self._prometheus_config(
                    dkms_pods=dkms_pods, sdn_pod=sdn_pod, link_pods=link_pods,
                ),
            },
        )
        self._upsert_config_map(
            self.loki_config_name,
            {"loki.yaml": self._loki_config()},
        )
        self._upsert_config_map(
            self.promtail_config_name,
            {"promtail.yaml": self._promtail_config()},
        )
        self._upsert_config_map(
            self.grafana_datasources_name,
            {"datasources.yaml": self._grafana_datasources()},
        )
        self._upsert_config_map(
            self.grafana_dashboards_provider_name,
            {"provider.yaml": self._grafana_dashboards_provider()},
        )
        self._upsert_config_map(
            self.grafana_dashboards_name,
            {
                "buffer-observability.json": self._grafana_dashboard_json(),
                "loadtest.json": self._grafana_loadtest_dashboard_json(),
            },
        )

        if K8S_OBS_PERSISTENCE_ENABLED:
            self._upsert_pvc(self.prometheus_pvc_name, K8S_OBS_PROM_PVC_SIZE)
            self._upsert_pvc(self.loki_pvc_name, K8S_OBS_LOKI_PVC_SIZE)
            self._upsert_pvc(self.grafana_pvc_name, K8S_OBS_GRAFANA_PVC_SIZE)

        self._upsert_service(
            name=self.prometheus_name,
            selector={"app": self.prometheus_name},
            port=K8S_OBS_PROMETHEUS_PORT,
        )
        self._upsert_service(
            name=self.loki_name,
            selector={"app": self.loki_name},
            port=K8S_OBS_LOKI_PORT,
        )
        self._upsert_service(
            name=self.grafana_name,
            selector={"app": self.grafana_name},
            port=K8S_OBS_GRAFANA_PORT,
        )

        prometheus_labels = {"app": self.prometheus_name}
        prometheus_pod_spec = client.V1PodSpec(
            containers=[
                client.V1Container(
                    name="prometheus",
                    image=K8S_OBS_PROMETHEUS_IMAGE,
                    image_pull_policy=image_pull_policy,
                    args=[
                        "--config.file=/etc/prometheus/prometheus.yml",
                        f"--storage.tsdb.retention.time={K8S_OBS_PROM_RETENTION}",
                    ],
                    ports=[
                        client.V1ContainerPort(container_port=K8S_OBS_PROMETHEUS_PORT),
                    ],
                    resources=client.V1ResourceRequirements(
                        requests={"cpu": "200m", "memory": "512Mi"},
                        limits={"cpu": "1500m", "memory": "3Gi"},
                    ),
                    volume_mounts=[
                        client.V1VolumeMount(
                            name="prometheus-config",
                            mount_path="/etc/prometheus",
                            read_only=True,
                        ),
                        client.V1VolumeMount(
                            name="prometheus-data",
                            mount_path="/prometheus",
                            read_only=False,
                        ),
                    ],
                )
            ],
            volumes=[
                client.V1Volume(
                    name="prometheus-config",
                    config_map=client.V1ConfigMapVolumeSource(name=self.prometheus_config_name),
                ),
                self._data_volume(
                    name="prometheus-data",
                    pvc_name=self.prometheus_pvc_name,
                ),
            ],
            image_pull_secrets=(
                [client.V1LocalObjectReference(name=image_pull_secret)]
                if image_pull_secret
                else None
            ),
            restart_policy="Always",
        )
        self._upsert_deployment(
            name=self.prometheus_name,
            labels=prometheus_labels,
            pod_spec=prometheus_pod_spec,
        )

        loki_labels = {"app": self.loki_name}
        loki_pod_spec = client.V1PodSpec(
            containers=[
                client.V1Container(
                    name="loki",
                    image=K8S_OBS_LOKI_IMAGE,
                    image_pull_policy=image_pull_policy,
                    args=["-config.file=/etc/loki/loki.yaml"],
                    ports=[client.V1ContainerPort(container_port=K8S_OBS_LOKI_PORT)],
                    resources=client.V1ResourceRequirements(
                        requests={"cpu": "200m", "memory": "512Mi"},
                        limits={"cpu": "1500m", "memory": "3Gi"},
                    ),
                    volume_mounts=[
                        client.V1VolumeMount(
                            name="loki-config",
                            mount_path="/etc/loki",
                            read_only=True,
                        ),
                        client.V1VolumeMount(
                            name="loki-data",
                            mount_path="/loki",
                            read_only=False,
                        ),
                    ],
                )
            ],
            volumes=[
                client.V1Volume(
                    name="loki-config",
                    config_map=client.V1ConfigMapVolumeSource(name=self.loki_config_name),
                ),
                self._data_volume(
                    name="loki-data",
                    pvc_name=self.loki_pvc_name,
                ),
            ],
            image_pull_secrets=(
                [client.V1LocalObjectReference(name=image_pull_secret)]
                if image_pull_secret
                else None
            ),
            restart_policy="Always",
        )
        self._upsert_deployment(
            name=self.loki_name,
            labels=loki_labels,
            pod_spec=loki_pod_spec,
        )

        grafana_labels = {"app": self.grafana_name}
        grafana_pod_spec = client.V1PodSpec(
            containers=[
                client.V1Container(
                    name="grafana",
                    image=K8S_OBS_GRAFANA_IMAGE,
                    image_pull_policy=image_pull_policy,
                    env=[
                        client.V1EnvVar(
                            name="GF_SERVER_ROOT_URL",
                            value=f"{_ingress_base_url()}/api/sim/{self.namespace}/grafana/",
                        ),
                        client.V1EnvVar(
                            name="GF_SERVER_SERVE_FROM_SUB_PATH",
                            value="true",
                        ),
                        client.V1EnvVar(
                            name="GF_AUTH_ANONYMOUS_ENABLED",
                            value="true",
                        ),
                        client.V1EnvVar(
                            name="GF_AUTH_ANONYMOUS_ORG_ROLE",
                            value=K8S_OBS_GRAFANA_ANON_ROLE,
                        ),
                        client.V1EnvVar(
                            name="GF_AUTH_DISABLE_LOGIN_FORM",
                            value="true",
                        ),
                    ],
                    ports=[client.V1ContainerPort(container_port=K8S_OBS_GRAFANA_PORT)],
                    resources=client.V1ResourceRequirements(
                        requests={"cpu": "200m", "memory": "256Mi"},
                        limits={"cpu": "1000m", "memory": "2Gi"},
                    ),
                    volume_mounts=[
                        client.V1VolumeMount(
                            name="grafana-datasources",
                            mount_path="/etc/grafana/provisioning/datasources",
                            read_only=True,
                        ),
                        client.V1VolumeMount(
                            name="grafana-dashboards-provider",
                            mount_path="/etc/grafana/provisioning/dashboards",
                            read_only=True,
                        ),
                        client.V1VolumeMount(
                            name="grafana-dashboards",
                            mount_path="/var/lib/grafana/dashboards",
                            read_only=True,
                        ),
                        client.V1VolumeMount(
                            name="grafana-data",
                            mount_path="/var/lib/grafana",
                            read_only=False,
                        ),
                    ],
                )
            ],
            volumes=[
                client.V1Volume(
                    name="grafana-datasources",
                    config_map=client.V1ConfigMapVolumeSource(
                        name=self.grafana_datasources_name
                    ),
                ),
                client.V1Volume(
                    name="grafana-dashboards-provider",
                    config_map=client.V1ConfigMapVolumeSource(
                        name=self.grafana_dashboards_provider_name
                    ),
                ),
                client.V1Volume(
                    name="grafana-dashboards",
                    config_map=client.V1ConfigMapVolumeSource(
                        name=self.grafana_dashboards_name
                    ),
                ),
                self._data_volume(
                    name="grafana-data",
                    pvc_name=self.grafana_pvc_name,
                ),
            ],
            image_pull_secrets=(
                [client.V1LocalObjectReference(name=image_pull_secret)]
                if image_pull_secret
                else None
            ),
            restart_policy="Always",
        )
        self._upsert_deployment(
            name=self.grafana_name,
            labels=grafana_labels,
            pod_spec=grafana_pod_spec,
        )


class PodLoadtest:
    """Deployments temporales que ejecutan ``loadtest.runner`` dentro del
    namespace de una simulación. Cada test es un Deployment independiente con
    label ``app=loadtest-runner`` y ``test_id=<id>``; Prometheus los descubre
    vía DNS-SD sobre el Service headless ``loadtest-runners``.
    """

    LABEL_APP = "loadtest-runner"
    HEADLESS_SERVICE = K8S_LOADTEST_SERVICE_NAME

    def __init__(self, id_simulation: str):
        self.namespace = str(id_simulation)
        try:
            config.load_incluster_config()
        except ConfigException:
            config.load_kube_config()
        self.core_v1_api = client.CoreV1Api()
        self.apps_v1_api = client.AppsV1Api()

    @staticmethod
    def deployment_name_for(test_id: str) -> str:
        safe = re.sub(r"[^a-z0-9-]+", "-", test_id.lower()).strip("-") or "t"
        return f"loadtest-{safe}"[:63]

    def _ensure_headless_service(self) -> None:
        body = client.V1Service(
            metadata=client.V1ObjectMeta(
                name=self.HEADLESS_SERVICE,
                labels={"app": self.LABEL_APP},
            ),
            spec=client.V1ServiceSpec(
                cluster_ip="None",  # headless → DNS-SD resolves to pod IPs
                selector={"app": self.LABEL_APP},
                ports=[
                    client.V1ServicePort(
                        name="metrics",
                        port=K8S_LOADTEST_METRICS_PORT,
                        target_port=K8S_LOADTEST_METRICS_PORT,
                    )
                ],
                publish_not_ready_addresses=True,
            ),
        )
        try:
            self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as exc:
            if exc.status != 409:
                raise

    OUTPUT_VOLUME_NAME = "loadtest-output"
    OUTPUT_MOUNT_PATH = "/var/loadtest-output"

    def _build_pod_spec(
        self,
        *,
        test_id: str,
        env_vars: Dict[str, str],
        image: str,
        image_pull_secret: Optional[str],
        image_pull_policy: str,
    ) -> client.V1PodSpec:
        env = [client.V1EnvVar(name=k, value=str(v)) for k, v in env_vars.items()]
        # emptyDir para que el runner escriba requests.csv / sae_timeline.csv
        # en un path estable que `kubectl cp` puede extraer después. Los
        # ficheros sobreviven mientras el pod viva; al borrar el
        # Deployment se pierden (es lo esperado — descárgalos antes).
        volume_mounts = [
            client.V1VolumeMount(
                name=self.OUTPUT_VOLUME_NAME,
                mount_path=self.OUTPUT_MOUNT_PATH,
            )
        ]
        container = client.V1Container(
            name="loadtest",
            image=image,
            image_pull_policy=image_pull_policy,
            env=env,
            ports=[
                client.V1ContainerPort(
                    name="metrics",
                    container_port=K8S_LOADTEST_METRICS_PORT,
                )
            ],
            resources=client.V1ResourceRequirements(
                requests={"cpu": "200m", "memory": "512Mi"},
                limits={"cpu": "2000m", "memory": "4Gi"},
            ),
            volume_mounts=volume_mounts,
        )
        volumes = [
            client.V1Volume(
                name=self.OUTPUT_VOLUME_NAME,
                empty_dir=client.V1EmptyDirVolumeSource(),
            )
        ]
        return client.V1PodSpec(
            containers=[container],
            restart_policy="Always",
            image_pull_secrets=(
                [client.V1LocalObjectReference(name=image_pull_secret)]
                if image_pull_secret
                else None
            ),
            volumes=volumes,
        )

    def deploy_test(
        self,
        *,
        test_id: str,
        env_vars: Dict[str, str],
        image: Optional[str] = None,
        image_pull_secret: Optional[str] = None,
        image_pull_policy: Optional[str] = None,
    ) -> str:
        if image is None:
            image = K8S_LOADTEST_IMAGE
        if image_pull_policy is None:
            image_pull_policy = K8S_IMAGE_PULL_POLICY

        self._ensure_headless_service()

        name = self.deployment_name_for(test_id)
        labels = {
            "app": self.LABEL_APP,
            "test_id": re.sub(r"[^a-z0-9-]+", "-", test_id.lower()).strip("-")[:63] or "t",
        }
        pod_spec = self._build_pod_spec(
            test_id=test_id,
            env_vars=env_vars,
            image=image,
            image_pull_secret=image_pull_secret,
            image_pull_policy=image_pull_policy,
        )
        body = client.V1Deployment(
            metadata=client.V1ObjectMeta(
                name=name,
                labels=labels,
                annotations={"dkms.test_id": test_id},
            ),
            spec=client.V1DeploymentSpec(
                replicas=1,
                selector=client.V1LabelSelector(match_labels=labels),
                template=client.V1PodTemplateSpec(
                    metadata=client.V1ObjectMeta(
                        labels=labels,
                        annotations={"dkms.test_id": test_id},
                    ),
                    spec=pod_spec,
                ),
            ),
        )
        try:
            self.apps_v1_api.create_namespaced_deployment(
                namespace=self.namespace,
                body=body,
            )
        except ApiException as exc:
            if exc.status != 409:
                raise
            self.apps_v1_api.replace_namespaced_deployment(
                name=name,
                namespace=self.namespace,
                body=body,
            )
        return name

    def delete_test(self, test_id: str) -> bool:
        name = self.deployment_name_for(test_id)
        try:
            self.apps_v1_api.delete_namespaced_deployment(
                name=name,
                namespace=self.namespace,
                body=client.V1DeleteOptions(propagation_policy="Background"),
            )
            return True
        except ApiException as exc:
            if exc.status == 404:
                return False
            raise

    def list_tests(self) -> list[dict]:
        try:
            deployments = self.apps_v1_api.list_namespaced_deployment(
                namespace=self.namespace,
                label_selector=f"app={self.LABEL_APP}",
            )
        except ApiException as exc:
            if exc.status == 404:
                return []
            raise
        out: list[dict] = []
        for d in deployments.items or []:
            meta = d.metadata or client.V1ObjectMeta()
            st = d.status or client.V1DeploymentStatus()
            out.append(
                {
                    "name": meta.name,
                    "test_id": (meta.annotations or {}).get("dkms.test_id")
                    or (meta.labels or {}).get("test_id")
                    or meta.name,
                    "replicas": int(getattr(st, "replicas", 0) or 0),
                    "ready_replicas": int(getattr(st, "ready_replicas", 0) or 0),
                    "available_replicas": int(getattr(st, "available_replicas", 0) or 0),
                    "created_at": (
                        meta.creation_timestamp.isoformat()
                        if getattr(meta, "creation_timestamp", None)
                        else None
                    ),
                }
            )
        return out



class PodSDN(Pod):

    __TYPE__ = "sdn"

    def __init__(self, id_simulation: str, model_sdn: ModelSDN):
        super().__init__(id_simulation, model_sdn)


class PodDKMS(Pod):
    __TYPE__ = "dkms"

    def __init__(self, id_simulation: str, model_dkms: ModelDKMS):
        super().__init__(id_simulation, model_dkms)
        # v3.3: el quditto-sidecar fue eliminado. Conservamos los métodos
        # `_extract_quditto_topology` y `_quditto_config_yaml` (líneas más
        # abajo) porque los REUTILIZA `PodQudittoLink` para construir su
        # config per-link. No hay state ni volumes específicos del sidecar
        # en este pod ya.

    def has_quditto_sidecar(self) -> bool:
        # Hard switch v3.3: ya no hay sidecar. Mantenido por compat con
        # callers existentes (orchestator decides flujo en base a esto).
        return False

    @staticmethod
    def _safe_int(value: Any) -> Optional[int]:
        try:
            parsed = int(value)
        except (TypeError, ValueError):
            return None
        if parsed <= 0:
            return None
        return parsed

    @staticmethod
    def _normalize_qkd_name(
        raw_name: Any,
        *,
        qkc_ip: Any = None,
        qkc_id: Any = None,
    ) -> str:
        text = str(raw_name or "").strip()
        if text:
            lowered = text.lower()
            if lowered.startswith("dkms-"):
                suffix = text.split("-", 1)[1].strip()
                suffix_int = _safe_int(suffix)
                if suffix_int is not None:
                    return f"DKMS-{int(suffix_int)}"
                return text

        inferred = _infer_node_id_from_ip(qkc_ip)
        if inferred is not None:
            return f"DKMS-{int(inferred)}"

        if text:
            numeric_text = _safe_int(text)
            if numeric_text is not None:
                return f"DKMS-{int(numeric_text)}"

        qkc_numeric = _safe_int(qkc_id)
        if qkc_numeric is not None:
            return f"DKMS-{int(qkc_numeric)}"

        return text

    def _dkms_service_ports(self) -> list[client.V1ServicePort]:
        app_port = self._service_port()
        ports: list[tuple[str, int]] = [("app", app_port)]
        seen_ports: set[int] = {app_port}
        # v3 ACK socket: el Generator abre un listener TCP raw para
        # frames FRAME_ACK en ``app_port + DKMS_ACK_SOCKET_PORT_OFFSET``
        # (default 1). Exponerlo en el Service para que los peers DKMS
        # puedan mandarnos ACKs cross-pod.
        ack_offset = self._safe_int(
            os.getenv("DKMS_ACK_SOCKET_PORT_OFFSET", "1")
        ) or 1
        ack_socket_port = app_port + ack_offset
        if ack_socket_port not in seen_ports:
            ports.append(("dkms-ack-socket", ack_socket_port))
            seen_ports.add(ack_socket_port)

        metrics_port = self._safe_int(K8S_QUDITTO_METRICS_PORT)
        if metrics_port and metrics_port not in seen_ports:
            ports.append(("quditto-metrics", metrics_port))
            seen_ports.add(metrics_port)

        model = getattr(self, "model", None)
        orr_model = getattr(model, "orr", None)
        if orr_model is not None:
            orr_host = getattr(orr_model, "host", None)
            orr_port = self._safe_int(getattr(orr_host, "port", None))
            if orr_port and orr_port not in seen_ports:
                ports.append(("orr", orr_port))
                seen_ports.add(orr_port)

            qkc_model = getattr(orr_model, "qkc", None)
            if qkc_model is not None:
                qkc_host = getattr(qkc_model, "host", None)
                qkc_port = self._safe_int(getattr(qkc_host, "port", None))
                if qkc_port and qkc_port not in seen_ports:
                    ports.append(("qkc", qkc_port))
                    seen_ports.add(qkc_port)
                    # v3 socket transport: el QKC abre un listener TCP raw
                    # adicional en `port + QKC_SOCKET_PORT_OFFSET` (default 1)
                    # para el hot path entre QKCs. Exponer el puerto en el
                    # Service permite que los peers conecten cross-pod.
                    socket_offset = self._safe_int(
                        os.getenv("QKC_SOCKET_PORT_OFFSET", "1")
                    ) or 1
                    qkc_socket_port = qkc_port + socket_offset
                    if qkc_socket_port not in seen_ports:
                        ports.append(("qkc-socket", qkc_socket_port))
                        seen_ports.add(qkc_socket_port)

                for kme in getattr(qkc_model, "kmes", []) or []:
                    pqc_enabled = bool(getattr(kme, "pqc_simulation", False)) or bool(
                        getattr(kme, "hybrid_enabled", False)
                    )
                    if not pqc_enabled:
                        continue
                    pqc_port = self._safe_int(getattr(kme, "pqc_kme_port", None))
                    if not pqc_port or pqc_port in seen_ports:
                        continue
                    ports.append((f"pqc-{pqc_port}", pqc_port))
                    seen_ports.add(pqc_port)

        return [
            client.V1ServicePort(name=name, port=port, target_port=port)
            for name, port in ports
        ]

    def _legacy_service_suffix(self) -> Optional[str]:
        cfg_id = self._resolve_dkms_config_id()
        if cfg_id is None:
            return None
        cfg_path = self._config_files_dir() / "DKMS" / f"{cfg_id}.json"
        if not cfg_path.is_file():
            return None
        try:
            payload = json.loads(cfg_path.read_text(encoding="utf-8"))
        except Exception:
            return None

        host_payload = payload.get("host")
        suffix: Any = None
        if isinstance(host_payload, dict):
            suffix = host_payload.get("id")
        if suffix is None:
            suffix = payload.get("id_host")
        if suffix is None:
            return None
        legacy = str(suffix).strip()
        if not legacy or legacy == self.name_suffix:
            return None
        return legacy

    def _create_service_object(
        self,
        *,
        name: str,
        service_ports: list[client.V1ServicePort],
    ) -> client.V1Service:
        return client.V1Service(
            metadata=client.V1ObjectMeta(name=name),
            spec=client.V1ServiceSpec(
                selector=self._workload_labels(),
                ports=service_ports,
                type="ClusterIP",
            ),
        )

    def _ensure_alias_service(
        self,
        *,
        alias_name: str,
        service_ports: list[client.V1ServicePort],
    ) -> None:
        existing = self._read_namespaced(self.core_v1_api.read_namespaced_service, alias_name)
        if existing:
            return
        alias = self._create_service_object(name=alias_name, service_ports=service_ports)
        try:
            self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=alias,
            )
        except ApiException as e:
            if e.status != 409:
                raise

    def create_service(self):
        existing = self._read_namespaced(self.core_v1_api.read_namespaced_service, self.name)
        if existing:
            return existing

        service_ports = self._dkms_service_ports()
        service = self._create_service_object(name=self.name, service_ports=service_ports)
        try:
            created = self.core_v1_api.create_namespaced_service(
                namespace=self.namespace,
                body=service,
            )
            legacy_suffix = self._legacy_service_suffix()
            if legacy_suffix:
                self._ensure_alias_service(
                    alias_name=f"{self.__TYPE__}-{legacy_suffix}",
                    service_ports=service_ports,
                )
            return created
        except ApiException as e:
            if e.status == 409:
                created = self._read_namespaced(self.core_v1_api.read_namespaced_service, self.name)
                legacy_suffix = self._legacy_service_suffix()
                if legacy_suffix:
                    self._ensure_alias_service(
                        alias_name=f"{self.__TYPE__}-{legacy_suffix}",
                        service_ports=service_ports,
                    )
                return created
            raise

    def _extract_qkc_id(self) -> Optional[int]:
        orr_model = getattr(self.model, "orr", None)
        if orr_model is not None:
            qkc_id = getattr(orr_model, "qkc_id", None)
            if qkc_id is not None:
                try:
                    return int(qkc_id)
                except (TypeError, ValueError):
                    return None
        return None

    def _extract_qkc_payload(self) -> Optional[Dict[str, Any]]:
        qkc_id = self._extract_qkc_id()
        model_id = _safe_int(getattr(self.model, "id", None))
        inferred_dkms_id = self._resolve_dkms_config_id()

        orr_model = getattr(self.model, "orr", None)
        if orr_model is not None:
            qkc_model = getattr(orr_model, "qkc", None)
            if qkc_model is not None and getattr(qkc_model, "kmes", None):
                try:
                    # mode="json" normaliza Enums a sus valores ("qkd", "pqc-simulation")
                    # y preserva parámetros por enlace editados desde la web.
                    return qkc_model.model_dump(mode="json", exclude_none=True)
                except Exception:
                    pass

        # Fallback: JSON canónicos de config_files (IDs 1..N de la topología de simulación).
        preferred_qkc_id = self._prefer_existing_config_id(
            "QKC",
            [inferred_dkms_id, qkc_id, model_id],
        )
        if preferred_qkc_id is not None:
            qkc_path = self._config_files_dir() / "QKC" / f"{preferred_qkc_id}.json"
            if qkc_path.is_file():
                try:
                    payload = json.loads(qkc_path.read_text(encoding="utf-8"))
                    if isinstance(payload.get("kmes"), list):
                        return payload
                except Exception:
                    pass

        if qkc_id is None:
            return None
        qkc_path = self._config_files_dir() / "QKC" / f"{qkc_id}.json"
        if not qkc_path.is_file():
            return None
        try:
            return json.loads(qkc_path.read_text(encoding="utf-8"))
        except Exception:
            return None

    def _extract_quditto_topology(self) -> tuple[str, list[dict[str, Any]]]:
        payload = self._extract_qkc_payload() or {}
        raw_kmes = payload.get("kmes")
        if not isinstance(raw_kmes, list):
            return "", []

        node_name = ""
        by_neighbor: Dict[str, Dict[str, Any]] = {}
        for item in raw_kmes:
            if not isinstance(item, dict):
                continue
            if not _is_qkd_link(item):
                continue

            local_name = self._normalize_qkd_name(
                item.get("local_qkd_id"),
                qkc_ip=item.get("local_qkc_ip"),
                qkc_id=item.get("local_qkc_id"),
            )
            if local_name and not node_name:
                node_name = local_name

            neighbor_name = self._normalize_qkd_name(
                item.get("neighbor_qkd_id"),
                qkc_ip=item.get("neighbor_qkc_ip"),
                qkc_id=item.get("neighbor_qkc_id"),
            )
            if not neighbor_name:
                continue

            channel = item.get("channel")
            ttl = K8S_QUDITTO_DEFAULT_TTL
            max_buffer_size = K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE
            distance = 0
            rate_r0 = K8S_QUDITTO_DEFAULT_RATE_R0
            rate_alpha = K8S_QUDITTO_DEFAULT_RATE_ALPHA
            if isinstance(channel, dict):
                try:
                    ttl_candidate = int(channel.get("ttl"))
                    if ttl_candidate > 0:
                        ttl = ttl_candidate
                except (TypeError, ValueError):
                    pass
                distance = max(0, _safe_int(channel.get("distance")) or 0)
                max_buffer_size = _safe_positive_int(
                    channel.get("max_buffer_size")
                    or channel.get("quditto_max_buffer_size"),
                    K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE,
                )
                raw_rate_r0 = channel.get("rate_r0")
                if raw_rate_r0 is None:
                    raw_rate_r0 = channel.get("quditto_rate_r0")
                raw_rate_alpha = channel.get("rate_alpha")
                if raw_rate_alpha is None:
                    raw_rate_alpha = channel.get("quditto_rate_alpha")
                rate_r0 = _safe_positive_float(
                    raw_rate_r0,
                    K8S_QUDITTO_DEFAULT_RATE_R0,
                )
                rate_alpha = _safe_nonnegative_float(
                    raw_rate_alpha,
                    K8S_QUDITTO_DEFAULT_RATE_ALPHA,
                )

            by_neighbor[neighbor_name] = {
                "name": neighbor_name,
                "role": _compute_quditto_role(item.get("local_qkc_id"), item.get("neighbor_qkc_id")),
                "ttl": ttl,
                "max_buffer_size": max_buffer_size,
                "distance": distance,
                "rate_r0": rate_r0,
                "rate_alpha": rate_alpha,
            }

        if not node_name:
            qkc_id = payload.get("id") or self._extract_qkc_id()
            if qkc_id is not None:
                node_name = f"QKD_{qkc_id}"

        return node_name, list(by_neighbor.values())

    # v3.3: `_quditto_config_yaml` de PodDKMS (per-node) fue eliminado.
    # `PodQudittoLink._quditto_config_yaml` genera la config per-link.

    def _upsert_config_map(self, name: str, data: Dict[str, str]):
        config_map = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(name=name),
            data=data,
        )
        try:
            return self.core_v1_api.create_namespaced_config_map(namespace=self.namespace, body=config_map)
        except ApiException as e:
            if e.status == 409:
                existing = self._read_namespaced(self.core_v1_api.read_namespaced_config_map, name)
                if existing is not None and existing.metadata is not None:
                    resource_version = getattr(existing.metadata, "resource_version", None)
                    if resource_version:
                        if config_map.metadata is None:
                            config_map.metadata = client.V1ObjectMeta(name=name)
                        config_map.metadata.resource_version = resource_version
                return self.core_v1_api.replace_namespaced_config_map(
                    name=name,
                    namespace=self.namespace,
                    body=config_map,
                )
            raise

    def crear_config_map(self):
        # v3.3: el ConfigMap del quditto-sidecar fue eliminado. La config
        # per-link la genera `PodQudittoLink` para su propio pod.
        base = super().crear_config_map()
        if K8S_DKMS_RUST_SIDECARS:
            # qkc.toml + orr.toml para los sidecars Rust.
            sidecar_data = {
                "qkc.toml": self._qkc_runtime_toml(),
                "orr.toml": self._orr_runtime_toml(),
            }
            self._upsert_config_map(self._sidecar_config_map_name(), sidecar_data)
        return base

    # ─── Rust sidecars (orr + qkc, opt-in via K8S_DKMS_RUST_SIDECARS) ─────
    _RUST_CERTS_VOLUME_NAME = "dkms-rust-certs"

    def _sidecar_config_map_name(self) -> str:
        return f"{self.name}-sidecars"

    def _sidecar_volume_name(self) -> str:
        return "rust-sidecars-config"

    def _safe_neighbor_host(self, neighbor_id: int) -> str:
        return f"dkms-{int(neighbor_id)}"

    def _resolve_orr_id_int(self) -> int:
        orr_model = getattr(self.model, "orr", None)
        if orr_model is not None:
            orr_id = (
                _safe_int(getattr(orr_model, "id", None))
                or _safe_int(getattr(orr_model, "qkc_id", None))
            )
            if orr_id:
                return int(orr_id)
        return (
            self._extract_qkc_id()
            or _safe_int(getattr(self.model, "id", None))
            or 1
        )

    def _qkc_runtime_toml(self) -> str:
        payload = self._extract_qkc_payload() or {}
        raw_kmes = payload.get("kmes") or []
        # We need the ORIGINAL qkc_id (e.g. 100001) for naming compatibility
        # with PodQudittoLink. The payload may have either the original
        # qkc_id or the post-normalization node_id (1..N). Prefer the
        # original from the model.
        qkc_id_int = (
            self._extract_qkc_id()
            or _safe_int(getattr(self.model, "id", None))
            or 1
        )
        lines: list[str] = [
            "# Generado por orchestrator/pods.py — no editar a mano",
            f"qkc_id       = {int(qkc_id_int)}",
            f'peer_listen  = "0.0.0.0:{K8S_QKC_TCP_PORT}"',
            'local_listen = "0.0.0.0:7100"',
            'admin_http   = "0.0.0.0:7200"',
            "",
        ]
        seen_neighbors: set[int] = set()
        for kme in raw_kmes:
            if not isinstance(kme, dict):
                continue
            neighbor_id = _safe_int(kme.get("neighbor_qkc_id"))
            if neighbor_id is None or neighbor_id in seen_neighbors:
                continue
            if neighbor_id == int(qkc_id_int):
                continue
            seen_neighbors.add(neighbor_id)
            key_size = _safe_positive_int(kme.get("key_size_bits"), 256)
            if key_size % 8 != 0:
                key_size = max(8, (key_size // 8) * 8)
            # neighbor_peer_addr: el QKC vecino vive dentro del DKMS pod
            # vecino. El Service del DKMS vecino se llama por su host_id
            # (no por qkc_id). Sacamos el host del kme.neighbor_qkc_ip
            # que ya lo trae `_canonicalize_runtime_dkms_payload`.
            neighbor_host = str(kme.get("neighbor_qkc_ip") or f"dkms-{neighbor_id}")
            # quditto_url: pod per-link con qkc_ids canónicos
            local_for_link = sorted([int(qkc_id_int), int(neighbor_id)])
            quditto_svc = f"quditto-link-{local_for_link[0]}-{local_for_link[1]}"
            lines.extend([
                "[[links]]",
                f"neighbor_id        = {int(neighbor_id)}",
                f'neighbor_peer_addr = "{neighbor_host}:{K8S_QKC_TCP_PORT}"',
                f'quditto_url        = "http://{quditto_svc}:5000"',
                f"key_size_bits      = {int(key_size)}",
                "",
            ])
        return "\n".join(lines)

    def _orr_runtime_toml(self) -> str:
        orr_id_int = self._resolve_orr_id_int()
        qkc_id_int = (
            self._extract_qkc_id()
            or _safe_int(getattr(self.model, "id", None))
            or 1
        )
        return (
            "# Generado por orchestrator/pods.py — no editar a mano\n"
            f'orr_id          = "orr-{int(orr_id_int)}"\n'
            f"qkc_id          = {int(qkc_id_int)}\n"
            'qkc_local_addr  = "127.0.0.1:7100"\n'
            f'grpc_addr       = "0.0.0.0:{K8S_ORR_GRPC_PORT}"\n'
            f'metrics_addr    = "0.0.0.0:{K8S_ORR_METRICS_PORT}"\n'
            "default_max_hops = 0\n"
        )

    def _orr_runtime_env(self) -> Dict[str, str]:
        orr_id_int = self._resolve_orr_id_int()
        qkc_id_int = (
            self._extract_qkc_id()
            or _safe_int(getattr(self.model, "id", None))
            or 1
        )
        # common::config::load_config usa Environment::with_prefix("ORR")
        # con separator("__"). Para que tanto el split del prefix como
        # de los campos nested funcione, TODOS los separadores entre
        # tokens deben ser "__". Por eso usamos lowercase ya que el
        # Environment normaliza a lowercase para hacer match con los
        # campos serde.
        sdn_endpoint_url = f"http://sdn-{self._resolve_sdn_suffix()}:3000"
        return {
            "RUST_LOG": K8S_ORR_RUST_LOG,
            "CONFIG_DIR": "/app/config/orr",
            "ORR__orr_id": f"orr-{int(orr_id_int)}",
            "ORR__qkc_id": str(int(qkc_id_int)),
            "ORR__qkc_local_addr": "127.0.0.1:7100",
            "ORR__grpc_addr": f"0.0.0.0:{K8S_ORR_GRPC_PORT}",
            "ORR__metrics_addr": f"0.0.0.0:{K8S_ORR_METRICS_PORT}",
            "ORR__sdn_url": sdn_endpoint_url,
            "ORR__default_max_hops": "0",
        }

    def _resolve_sdn_suffix(self) -> int:
        """Best-effort: find the SDN host_id from the simulation namespace.

        SDN pod/service is named `sdn-{host_id}` (e.g. sdn-100030). We
        don't have direct access to the SDN model here, so we just list
        services in our namespace matching `sdn-*` and pick the first one.
        Falls back to host_id+10 if no SDN service is found yet.
        """
        try:
            services = self.core_v1_api.list_namespaced_service(self.namespace)
            for svc in (services.items or []):
                name = (svc.metadata.name if svc.metadata else "") or ""
                if name.startswith("sdn-") and name[4:].isdigit():
                    return int(name[4:])
        except Exception:  # noqa: BLE001
            pass
        return 0

    def _dkms_rust_env_overrides(
        self,
        *,
        sdn_host: Optional[str],
        sdn_port: Optional[str],
    ) -> Dict[str, str]:
        sae_port = self._service_port()
        peer_addr_port = sae_port + 1
        if not sdn_host:
            sdn_host = f"sdn-{self._resolve_sdn_suffix()}"
        sdn_port_int = _safe_positive_int(sdn_port, 3000)
        sdn_endpoint = f"http://{sdn_host}:{sdn_port_int}"
        # common::config::load_config usa Environment::with_prefix("DKMS")
        # con separator("__"). TODOS los separadores entre tokens deben
        # ser "__" (incluido el que separa el prefix del primer campo).
        return {
            "RUST_LOG": K8S_DKMS_RUST_LOG,
            "CONFIG_DIR": "/app/config/dkms",
            "DKMS__node_id": self.name,
            "DKMS__listen__sae_addr": f"0.0.0.0:{int(sae_port)}",
            "DKMS__listen__peer_addr": f"0.0.0.0:{int(peer_addr_port)}",
            "DKMS__listen__grpc_addr": f"0.0.0.0:{K8S_DKMS_RUST_GRPC_PORT}",
            "DKMS__listen__metrics_addr": f"0.0.0.0:{K8S_DKMS_RUST_METRICS_PORT}",
            "DKMS__southbound__sdn_endpoint": sdn_endpoint,
            "DKMS__southbound__qkc_endpoint": f"http://127.0.0.1:{K8S_QKC_GRPC_PORT}",
            "DKMS__southbound__orr_endpoint": f"http://127.0.0.1:{K8S_ORR_GRPC_PORT}",
            "DKMS__tls__cert_path": "/app/certs/server.crt",
            "DKMS__tls__key_path": "/app/certs/server.key",
            "DKMS__tls__sae_client_ca": "/app/certs/ca.crt",
            "DKMS__tls__peer_dkms_ca": "/app/certs/ca.crt",
        }

    def _orr_container(self, image: str, image_pull_policy: str) -> client.V1Container:
        env = [client.V1EnvVar(name=k, value=v) for k, v in self._orr_runtime_env().items()]
        return client.V1Container(
            name="orr",
            image=image,
            image_pull_policy=image_pull_policy,
            env=env,
            ports=[
                client.V1ContainerPort(container_port=K8S_ORR_GRPC_PORT),
                client.V1ContainerPort(container_port=K8S_ORR_METRICS_PORT),
            ],
            resources=client.V1ResourceRequirements(
                requests={"cpu": "100m", "memory": "128Mi"},
                limits={"cpu": "1000m", "memory": "512Mi"},
            ),
            startup_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_ORR_GRPC_PORT),
                period_seconds=2, timeout_seconds=2, failure_threshold=60,
            ),
            readiness_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_ORR_GRPC_PORT),
                period_seconds=5, timeout_seconds=2, failure_threshold=6,
            ),
            volume_mounts=[
                client.V1VolumeMount(
                    name=self._sidecar_volume_name(),
                    mount_path="/app/config/orr/default.toml",
                    sub_path="orr.toml",
                    read_only=True,
                ),
            ],
        )

    def _qkc_container(self, image: str, image_pull_policy: str) -> client.V1Container:
        return client.V1Container(
            name="qkc",
            image=image,
            image_pull_policy=image_pull_policy,
            command=["/usr/local/bin/qkc", "--config", "/app/config/qkc/qkc.toml"],
            env=[client.V1EnvVar(name="RUST_LOG", value=K8S_QKC_RUST_LOG)],
            ports=[
                client.V1ContainerPort(container_port=K8S_QKC_TCP_PORT),
                client.V1ContainerPort(container_port=7100),
                client.V1ContainerPort(container_port=7200),
            ],
            resources=client.V1ResourceRequirements(
                requests={"cpu": "100m", "memory": "128Mi"},
                limits={"cpu": "1500m", "memory": "1Gi"},
            ),
            startup_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_QKC_TCP_PORT),
                period_seconds=2, timeout_seconds=2, failure_threshold=60,
            ),
            readiness_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_QKC_TCP_PORT),
                period_seconds=5, timeout_seconds=2, failure_threshold=6,
            ),
            volume_mounts=[
                client.V1VolumeMount(
                    name=self._sidecar_volume_name(),
                    mount_path="/app/config/qkc/qkc.toml",
                    sub_path="qkc.toml",
                    read_only=True,
                ),
            ],
        )

    def _sidecar_volume(self) -> client.V1Volume:
        return client.V1Volume(
            name=self._sidecar_volume_name(),
            config_map=client.V1ConfigMapVolumeSource(
                name=self._sidecar_config_map_name(),
            ),
        )

    def _cert_init_container(self) -> client.V1Container:
        """Generates self-signed TLS material for the Rust DKMS sidecars.

        v3.4: replaces what the quditto sidecar (now per-link pod) used to
        generate. Per-pod self-signed — peer DKMS↔DKMS does NOT verify
        (TODO: extend runtime_ca to mint per-DKMS leafs).
        """
        script = (
            'set -e; '
            'cd /app/certs; '
            'openssl req -x509 -newkey rsa:2048 -keyout server.key -out server.crt '
            f'-days 365 -nodes -subj "/CN={self.name}" '
            f'-addext "subjectAltName=DNS:{self.name},DNS:localhost,IP:127.0.0.1"; '
            'cp server.crt ca.crt; '
            'chmod 644 server.crt server.key ca.crt'
        )
        return client.V1Container(
            name="cert-init",
            image="alpine/openssl:latest",
            image_pull_policy="IfNotPresent",
            command=["sh", "-c", script],
            volume_mounts=[
                client.V1VolumeMount(
                    name=self._RUST_CERTS_VOLUME_NAME,
                    mount_path="/app/certs",
                    read_only=False,
                ),
            ],
        )
    # ─── end Rust sidecars ────────────────────────────────────────────────

    def _build_runtime_env_payload(self) -> Dict[str, str]:
        """Construye el env_payload común para el container DKMS.

        v3.3: simplificado tras eliminar el quditto-sidecar (las vars
        QUDITTO_LOCAL_URL, QUDITTO_CLIENT_CERT_PATH, etc. ya no son
        necesarias — cada KME tiene su URL per-link en su config JSON).
        """
        env_payload: Dict[str, str] = {
            "CONFIG_FOLDER": "/app/config",
            "LOG_DIR": "/app/logs",
            "BIND_IP": "0.0.0.0",
            "AGENT_CONTROLLER_PORT": "8080",
            "DKMS_ADVERTISED_HOST": self.name,
            "KME_HTTP_TIMEOUT_SECONDS": K8S_DKMS_KME_HTTP_TIMEOUT_SECONDS,
            "KME_ENC_KEYS_REQUEST_TIMEOUT_SECONDS": K8S_DKMS_KME_ENC_KEYS_REQUEST_TIMEOUT_SECONDS,
            "KME_ENC_KEYS_RETRY_WINDOW_SECONDS": K8S_DKMS_KME_ENC_KEYS_RETRY_WINDOW_SECONDS,
            "KME_ENC_KEYS_RETRY_INTERVAL_SECONDS": K8S_DKMS_KME_ENC_KEYS_RETRY_INTERVAL_SECONDS,
            "KME_DEC_KEYS_RETRY_WINDOW_SECONDS": K8S_DKMS_KME_DEC_KEYS_RETRY_WINDOW_SECONDS,
            "KME_DEC_KEYS_RETRY_INTERVAL_SECONDS": K8S_DKMS_KME_DEC_KEYS_RETRY_INTERVAL_SECONDS,
            "QKC_DEC_KEYS_RETRY_WINDOW_SECONDS": K8S_DKMS_QKC_DEC_KEYS_RETRY_WINDOW_SECONDS,
            "QKC_SERVER_DECRYPT_RETRY_WINDOW_SECONDS": K8S_DKMS_QKC_SERVER_DECRYPT_RETRY_WINDOW_SECONDS,
            "QKC_RELAY_FORWARD_TIMEOUT_SECONDS": K8S_DKMS_QKC_RELAY_FORWARD_TIMEOUT_SECONDS,
            "QKC_SEND_TIMEOUT_SECONDS": K8S_DKMS_QKC_SEND_TIMEOUT_SECONDS,
            "PQC_SOCKET_TIMEOUT_SECONDS": K8S_DKMS_PQC_SOCKET_TIMEOUT_SECONDS,
            "DKMS_EXT_KEYS_RETRY_WINDOW_SECONDS": K8S_DKMS_EXT_KEYS_RETRY_WINDOW_SECONDS,
            "DKMS_EXT_KEYS_RETRY_INTERVAL_SECONDS": K8S_DKMS_EXT_KEYS_RETRY_INTERVAL_SECONDS,
            "DKMS_REQUIRE_CLIENT_CERT_IDENTITY": K8S_DKMS_REQUIRE_CLIENT_CERT_IDENTITY,
            "DKMS_REQUIRE_DEC_KEYS_PATH_MATCH": K8S_DKMS_REQUIRE_DEC_KEYS_PATH_MATCH,
            "DKMS_TRUSTED_PROXY_IPS": K8S_DKMS_TRUSTED_PROXY_IPS,
            "QKC_ENABLE_TOKEN_BUCKET": K8S_QKC_ENABLE_TOKEN_BUCKET,
            "DKMS_BUFFER_GENERATION_INTERVAL_SECONDS": K8S_DKMS_BUFFER_GENERATION_INTERVAL_SECONDS,
            "DKMS_SAE_BUFFER_OBSERVATION_WINDOW_SECONDS": K8S_DKMS_SAE_BUFFER_OBSERVATION_WINDOW_SECONDS,
            "DKMS_SAE_BUCKET_RETRY_AFTER_CEILING_SECONDS": K8S_DKMS_SAE_BUCKET_RETRY_AFTER_CEILING_SECONDS,
            "DKMS_GENERATOR_REFILL_DEMAND_KEYS_PER_SECOND": K8S_DKMS_GENERATOR_REFILL_DEMAND_KEYS_PER_SECOND,
            "DKMS_GENERATOR_MAX_CONCURRENT_GENERATIONS": K8S_DKMS_GENERATOR_MAX_CONCURRENT_GENERATIONS,
            "DKMS_ASYNC_SEND_WORKERS": K8S_DKMS_ASYNC_SEND_WORKERS,
            "KME_ENC_PREFETCH_COUNT": K8S_KME_ENC_PREFETCH_COUNT,
            "KME_ENC_KEYS_BATCH_SIZE": K8S_KME_ENC_KEYS_BATCH_SIZE,
            "DKMS_MAX_IN_FLIGHT_KEYS_PER_PEER": K8S_DKMS_MAX_IN_FLIGHT_KEYS_PER_PEER,
            "DKMS_ACK_TIMEOUT_SECONDS": K8S_DKMS_ACK_TIMEOUT_SECONDS,
        }
        # v3.3: no más QUDITTO_* env vars — quditto es pod independiente
        # accesible vía la URL per-link en cada KME config.
        # v2.4 async decrypt: el receiver usa un único httpx.AsyncClient
        # compartido con cap de concurrencia por semaphore. Diagnóstico
        # cluster v2.3.x con QKC_DECRYPT_PROCESS_WORKERS=16:
        # qkc_recv_duration_seconds{decrypt} >10s en el 84% de los lotes
        # mientras qkc_dec_keys_latency_seconds (HTTP real) ~32ms. El
        # cuello no era el sidecar sino la cola del ProcessPool +
        # serialize/IPC. async I/O escala mejor: 100 inflight ~100KB heap
        # vs 100 procesos ~1GB y thrashing.
        # Override env: QKC_DECRYPT_ASYNC_CONCURRENCY (default 128 v2.4.4).
        # Cluster v2.4.3 medía qkc_decrypt_inflight=408 con cap=32 (cola
        # de 376 esperando entrar al sidecar local que aceptaba 32 a la vez).
        # Con cap=128: inflight=0 (sin cola), recv decrypt 39s → 3.6s (10×).
        # KEEPALIVE=128 mantiene las conexiones del pool todas vivas
        # — bajarlo a 4 (probado) introduce TLS handshake per-request y
        # empeora 50%. Con sidecar 8 workers, 128 conexiones se
        # distribuyen ~16/worker via SO_REUSEPORT al startup.
        # Para volver al ProcessPool poner QKC_DECRYPT_ASYNC=0 y
        # QKC_DECRYPT_PROCESS_WORKERS=N.
        # F12+ default OFF: async re-introducía el cuello del event loop
        # (p50=4.6s bajo carga). Forzar process_pool path.
        env_payload["QKC_DECRYPT_ASYNC"] = os.getenv("QKC_DECRYPT_ASYNC", "0")
        env_payload["QKC_DECRYPT_ASYNC_CONCURRENCY"] = os.getenv(
            "QKC_DECRYPT_ASYNC_CONCURRENCY", "128",
        )
        env_payload["QKC_DECRYPT_ASYNC_KEEPALIVE"] = os.getenv(
            "QKC_DECRYPT_ASYNC_KEEPALIVE", "128",
        )
        # v2.5: probe de capacity efectiva del enlace QKC↔QKC. Cada QKC
        # mide su throughput real y reporta a la SDN si supera deadband.
        # Defaults conservadores para no saturar la SDN (ver QKC.py).
        env_payload["QKC_LINK_CAPACITY_PROBE"] = os.getenv("QKC_LINK_CAPACITY_PROBE", "1")
        env_payload["QKC_LINK_CAPACITY_PROBE_INTERVAL"] = os.getenv(
            "QKC_LINK_CAPACITY_PROBE_INTERVAL", "30",
        )
        env_payload["QKC_LINK_CAPACITY_DEADBAND"] = os.getenv(
            "QKC_LINK_CAPACITY_DEADBAND", "0.15",
        )
        env_payload["QKC_LINK_CAPACITY_BOOTSTRAP_SECONDS"] = os.getenv(
            "QKC_LINK_CAPACITY_BOOTSTRAP_SECONDS", "60",
        )
        # v2.5.3: histeresis de notificación de QoS (paper-style T_f).
        # c=3.0 espera el doble que default (1.5) para no saturar la SDN
        # con PATCHs cuando los buffers oscilan. T_min=2s evita ráfagas
        # de PATCHs <1s. T_max=60s techo absoluto. v2.6 añade skip
        # histeresis para BEST_EFFORT (urgente, ver keys_generator.py).
        env_payload["DKMS_HYST_C"] = os.getenv("DKMS_HYST_C", "3.0")
        env_payload["DKMS_HYST_T_MIN"] = os.getenv("DKMS_HYST_T_MIN", "2.0")
        env_payload["DKMS_HYST_T_MAX"] = os.getenv("DKMS_HYST_T_MAX", "60.0")
        # v2.6.1: bypass debouncer DESACTIVADO por default. La idea era
        # propagar PATCHs BEST_EFFORT inmediatamente al MCF para reaccionar
        # rápido a buffers saturados. Cluster real (50 DKMSs, cap=500):
        # ~50 PATCHs BEST_EFFORT/s × 4s/recompute = SDN saturada en bucle
        # → restart. El debouncer normal (max_wait=8s) absorbe los PATCHs
        # y agrupa, manteniendo ratio PATCH:recompute en ~42:1. Si en
        # un cluster pequeño se prueba con bypass=1, OK; en producción 0.
        env_payload["SDN_DEBOUNCE_BYPASS_BEST_EFFORT"] = os.getenv(
            "SDN_DEBOUNCE_BYPASS_BEST_EFFORT", "0",
        )
        # F19: subido a 16 (era 4 desde F13). Decrypt path estaba al 10%
        # de su cap (414/s observed vs 1666/s cap). Subir procs no aumenta
        # cap si decrypt no está saturado, pero asegura headroom para
        # cargas mayores tras F19's send_workers=128.
        env_payload["QKC_DECRYPT_PROCESS_WORKERS"] = os.getenv(
            "QKC_DECRYPT_PROCESS_WORKERS", "16",
        )
        # Generator concurrency. send_workers=16 (validado v2.3.8).
        # MAX_IN_FLIGHT=256 (v2.4.8): el cap=24 antiguo tenía sentido
        # cuando el receiver tardaba 5-10s/POST y el sem total
        # serializaba 24 inflight. Con sidecar HTTP plano + cap_per_peer
        # reducido (v2.4.7) el receiver tarda ~70 ms/op; cap=24 era el
        # nuevo cuello (peer 11 acapara los 24 slots → otros 49 peers
        # esperan → 16 peers con emit=0/s). Subir a 256 elimina la
        # serialización: gen_emit 35 → 87 keys/s/DKMS (+148%),
        # buf_generated 19 → 34 keys/s (+79%), peers con emit=0
        # bajan de 16 a 0 (todos emiten).
        # Iter 11: rollback iter 10 (32→16 workers no ayudó: vacíos
        # subió 56→63%). Bottleneck NO es número de workers sino DOWNSTREAM
        # (sidecar quditto). Iter 11 ataca sidecar: QUDITTO_WORKERS 4→8
        # via env separada (línea ~60).
        # F19: 16→128 send_workers. Cada send es ~6ms (encrypt+post) y los
        # workers están idle el 99% del tiempo bajo demand actual. Más
        # workers permite servir bursts del scheduler sin queuear submits.
        env_payload["DKMS_GENERATOR_SEND_WORKERS"] = os.getenv(
            "DKMS_GENERATOR_SEND_WORKERS", "128",
        )
        env_payload["DKMS_GENERATOR_MAX_IN_FLIGHT"] = os.getenv(
            "DKMS_GENERATOR_MAX_IN_FLIGHT", "256",
        )
        # v2.10.8 final: LINK_STORE off (drain `all_keys=true` cada 100ms
        # saturó CPU y regresionó throughput). Confiar en
        # `_enc_prefetch_worker` con KME_ENC_PREFETCH_COUNT=512 y
        # KME_ENC_KEYS_BATCH_SIZE=64.
        env_payload["KME_LINK_STORE_ENABLED"] = os.getenv(
            "KME_LINK_STORE_ENABLED", "false",
        )
        # Patch F12: parallel decrypt activo. Drain on-demand DESACTIVADO
        # por default tras medición F12: en cluster real saturaba quditto
        # (latencia 7s/req). Activable manualmente para escenarios de
        # tráfico bajo o aislamiento.
        env_payload["KME_LINK_STORE_NO_PERIODIC"] = os.getenv(
            "KME_LINK_STORE_NO_PERIODIC", "1",
        )
        env_payload["QKC_RECV_PARALLEL"] = os.getenv(
            "QKC_RECV_PARALLEL", "0",
        )
        env_payload["QKC_RECV_PARALLELISM"] = os.getenv(
            "QKC_RECV_PARALLELISM", "64",
        )
        env_payload["QKC_DEC_DRAIN_ON_MISS"] = os.getenv(
            "QKC_DEC_DRAIN_ON_MISS", "0",
        )
        env_payload["QKC_DEC_DRAIN_ON_MISS_INTERVAL_MS"] = os.getenv(
            "QKC_DEC_DRAIN_ON_MISS_INTERVAL_MS", "20",
        )
        env_payload["KME_LINK_STORE_DRAIN_INTERVAL_MS"] = os.getenv(
            "KME_LINK_STORE_DRAIN_INTERVAL_MS", "100",
        )
        env_payload["KME_LINK_STORE_DRAIN_TARGET_KEYS"] = os.getenv(
            "KME_LINK_STORE_DRAIN_TARGET_KEYS", "500",
        )
        env_payload["KME_LINK_STORE_DRAIN_MIN_MS"] = os.getenv(
            "KME_LINK_STORE_DRAIN_MIN_MS", "50",
        )
        env_payload["KME_LINK_STORE_DRAIN_MAX_MS"] = os.getenv(
            "KME_LINK_STORE_DRAIN_MAX_MS", "1000",
        )
        env_payload["KME_DEC_PREFETCH_CACHE_MAX"] = os.getenv(
            "KME_DEC_PREFETCH_CACHE_MAX", "20000",
        )
        # F26: max_tokens=512 + batch=64 + cap=10000. Con buffer grande
        # (F25) no hay oscilación SATURATED, así que el peer hot puede
        # drenar tokens sin penalizar a los lentos (que reciben más rate
        # cuando los hot saturan). max_tokens=512 = cap 20480 keys/s/peer
        # con tick=25ms.
        env_payload["DKMS_GEN_MAX_TOKENS_PER_PEER_PER_TICK"] = os.getenv(
            "DKMS_GEN_MAX_TOKENS_PER_PEER_PER_TICK", "512",
        )
        env_payload["DKMS_GENERATOR_BATCH_SIZE"] = os.getenv(
            "DKMS_GENERATOR_BATCH_SIZE", "64",
        )
        # F14: ENCRYPT_PROCESS_WORKERS=0 (deshabilitado de nuevo). El path
        # subprocess hace HTTP enc_keys directamente al sidecar quditto sin
        # tocar el _enc_prefetch_queue del parent. Bajo conc=64 + single-key
        # inflation (size=6936) quditto cae a 72 calls/s/sidecar = 1000
        # generations/s cluster (10% de O1). Con workers=0 el path in-thread
        # llama kme_request_enc_key() que SÍ usa prefetch en O(1).
        env_payload["QKC_ENCRYPT_PROCESS_WORKERS"] = os.getenv(
            "QKC_ENCRYPT_PROCESS_WORKERS", "0",
        )
        # F14: single-key inflation OFF (=0). El default 8192 inflaba 1 key
        # del tamaño del payload entero (~6936 bits) ahorrando key_ids en
        # el frame, pero size!=256 desactivaba el prefetch (ver
        # KME._pop_prefetched_enc_key: returns None si size_bits != 256).
        # Con SINGLE_KEY_MAX_BITS=0 los chunks van a 256 bits → cada chunk
        # popea del prefetch. El receiver decrypt agrupa todos los key_IDs
        # en 1 POST dec_keys (cap _DEC_KEYS_BATCH_CAP=128) → sigue siendo
        # 1 HTTP por mensaje.
        env_payload["QKC_ENCRYPT_SINGLE_KEY_MAX_BITS"] = os.getenv(
            "QKC_ENCRYPT_SINGLE_KEY_MAX_BITS", "0",
        )
        # Cap del buffer DKMS desacoplado del cap del sidecar quditto.
        # Antes el DKMS heredaba quditto_max_buffer_size del KME config
        # (ver DKMS_2.py:_resolve_buffers_max_keys), lo que causaba
        # deadlock cuando ambos caps coincidían y el priming inicial los
        # llevaba al 100% — clase SATURATED → solver excluye → rate=0.
        # Quditto sidecar = 10000, DKMS = 3000 (target.buffers.proyect.md §3).
        # F25: buffer cap 3000→10000. Con 3000 las transiciones SATURATED
        # (95% del cap) son frecuentes durante warmup, causando oscilación
        # en el SDN solver — demand_rate observado para flujos lentos
        # saltaba 287/s → 886/s → 0 → 308/s → 400/s → 0 caóticamente.
        # Con cap=10000 (== quditto sidecar) la SATURATED solo dispara
        # cuando el sistema está verdaderamente saturado, no en transitorio.
        env_payload["DKMS_BUFFERS_MAX_KEYS"] = os.getenv(
            "DKMS_BUFFERS_MAX_KEYS", "10000",
        )
        # ACK path speedup. Defaults conservadores (TTL=30s, flush=50ms,
        # batch=32, queues=8192) hacían que bajo carga real los ACKs
        # tardaran más que el TTL → keys expiraban en _ack_pending sin
        # confirmarse → ENC se quedaba muy por debajo de DEC. Con estos
        # valores ENC ≈ DEC en steady state (drift mean -12 medido).
        env_payload["DKMS_GENERATOR_ACK_TIMEOUT_SECONDS"] = os.getenv(
            "DKMS_GENERATOR_ACK_TIMEOUT_SECONDS", "600",
        )
        env_payload["DKMS_ACK_BATCH_FLUSH_INTERVAL_MS"] = os.getenv(
            "DKMS_ACK_BATCH_FLUSH_INTERVAL_MS", "10",
        )
        env_payload["DKMS_ACK_BATCH_MAX_KEYS"] = os.getenv(
            "DKMS_ACK_BATCH_MAX_KEYS", "128",
        )
        env_payload["DKMS_ACK_SOCKET_QUEUE_MAX"] = os.getenv(
            "DKMS_ACK_SOCKET_QUEUE_MAX", "65536",
        )
        env_payload["QKC_SOCKET_SEND_QUEUE_MAX"] = os.getenv(
            "QKC_SOCKET_SEND_QUEUE_MAX", "65536",
        )
        if os.getenv("PERSISTENCE_BACKEND"):
            env_payload["PERSISTENCE_BACKEND"] = str(os.getenv("PERSISTENCE_BACKEND"))
        if os.getenv("DB_URL"):
            env_payload["DB_URL"] = str(os.getenv("DB_URL"))
        # Propagate SQLAlchemy pool knobs so a 50-replica DKMS deployment
        # does not blow up ``max_connections`` on the shared RDS. Default
        # (5 + 10) × 50 = 750 slots peak; setting these on the orchestator
        # deployment (e.g. SQLALCHEMY_POOL_SIZE=2, SQLALCHEMY_MAX_OVERFLOW=3)
        # cuts the ceiling without a code change.
        for var in (
            "SQLALCHEMY_POOL_SIZE",
            "SQLALCHEMY_MAX_OVERFLOW",
            "SQLALCHEMY_POOL_TIMEOUT_SECONDS",
            "SQLALCHEMY_POOL_RECYCLE_SECONDS",
            "SQLALCHEMY_POOL_PRE_PING",
            "SQLALCHEMY_POOL_USE_LIFO",
            "SQLALCHEMY_CONNECT_TIMEOUT_SECONDS",
        ):
            value = os.getenv(var)
            if value:
                env_payload[var] = str(value)
        self._populate_dkms_config_env(env_payload)
        return env_payload

    def create_pod(
        self,
        env_vars: dict | None = None,
        image: str | None = None,
        ports: list | None = None,
        image_pull_secret: str | None = None,
        image_pull_policy: str | None = None,
        quditto_image: str | None = None,  # ← ignored, kept for backwards-compat signature
        orr_image: str | None = None,
        qkc_image: str | None = None,
    ):
        # v3.3: el quditto ya no es sidecar — se despliega en pods
        # independientes per-link (ver `PodQudittoLink`). El parámetro
        # `quditto_image` se acepta por compat con callers existentes
        # pero no se usa en este path.
        existing = self._read_namespaced(self.apps_v1_api.read_namespaced_deployment, self.name)
        if existing:
            return existing

        if image is None:
            image = f"{self.__TYPE__}:latest"
        if not image_pull_policy:
            image_pull_policy = K8S_IMAGE_PULL_POLICY
        if K8S_DKMS_RUST_SIDECARS:
            if orr_image is None:
                orr_image = K8S_ORR_IMAGE
            if qkc_image is None:
                qkc_image = K8S_QKC_IMAGE

        host_port = None
        host = getattr(self.model, "host", None)
        if host is not None:
            host_port = getattr(host, "port", None)

        env_payload = self._build_runtime_env_payload()
        if env_vars:
            for key, value in env_vars.items():
                env_payload[str(key)] = str(value)
        if K8S_DKMS_RUST_SIDECARS:
            rust_overrides = self._dkms_rust_env_overrides(
                sdn_host=env_payload.get("DKMS_SDN_HOST"),
                sdn_port=env_payload.get("DKMS_SDN_PORT"),
            )
            env_payload.update(rust_overrides)
        env_defaults = [client.V1EnvVar(name=key, value=value) for key, value in env_payload.items()]

        container_port = host_port or 8080
        container_ports = [client.V1ContainerPort(container_port=container_port)]
        if container_port != 8080:
            container_ports.append(client.V1ContainerPort(container_port=8080))
        if ports:
            for port in ports:
                container_ports.append(client.V1ContainerPort(container_port=port))

        if image_pull_secret is None and DOCKER_HUB_TOKEN:
            image_pull_secret = DOCKER_HUB_SECRET_NAME
            self._ensure_image_pull_secret()

        main_volume_mounts: list[client.V1VolumeMount] = []
        base_mount = self.crear_volume_mount()
        if base_mount is not None:
            main_volume_mounts.append(base_mount)
        main_volume_mounts.append(
            client.V1VolumeMount(
                name=self._logs_volume_name(),
                mount_path="/app/logs",
                read_only=False,
            )
        )
        if K8S_DKMS_RUST_SIDECARS:
            main_volume_mounts.append(
                client.V1VolumeMount(
                    name=self._RUST_CERTS_VOLUME_NAME,
                    mount_path="/app/certs",
                    read_only=True,
                )
            )

        dkms_container = client.V1Container(
            name="dkms",
            image=image,
            image_pull_policy=image_pull_policy,
            env=env_defaults,
            resources=client.V1ResourceRequirements(
                # CPU request 200m permitía a Karpenter packear DKMS en c5a.large
                # (2 CPU). Bajo carga real cada pod usa ~1 CPU sostenido (limit
                # 1500m). Subo request a 750m: Karpenter ya no mete DKMSs en
                # c5a.large/medium, fuerza nodos ≥xlarge. Memoria: peak RSS
                # observado 1073MB; subo request 512→1Gi para reflejarlo.
                requests={"cpu": "750m", "memory": "1Gi"},
                limits={"cpu": "1500m", "memory": "2Gi"},
            ),
            ports=container_ports,
            volume_mounts=main_volume_mounts,
            startup_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=container_port),
                period_seconds=2,
                timeout_seconds=2,
                failure_threshold=60,
            ),
            readiness_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=container_port),
                period_seconds=5,
                timeout_seconds=2,
                failure_threshold=6,
            ),
        )

        # v3.3: el sidecar `quditto` ya no se monta dentro del pod DKMS.
        # Cada link QKD vive en su propio pod `quditto-link-{a}-{b}` y los
        # KMEs apuntan a él vía DNS K8s (ver `generate_configs.py` y
        # `PodQudittoLink`). El pod DKMS queda con solo `dkms` (+ promtail
        # sidecar si observability está activa).

        volumes: list[client.V1Volume] = []
        base_volume = self.crear_volume()
        if base_volume is not None:
            volumes.append(base_volume)
        volumes.append(
            client.V1Volume(
                name=self._logs_volume_name(),
                empty_dir=client.V1EmptyDirVolumeSource(),
            )
        )

        containers = [dkms_container]
        init_containers: list[client.V1Container] = []
        if K8S_DKMS_RUST_SIDECARS:
            volumes.append(
                client.V1Volume(
                    name=self._RUST_CERTS_VOLUME_NAME,
                    empty_dir=client.V1EmptyDirVolumeSource(),
                )
            )
            volumes.append(self._sidecar_volume())
            init_containers.append(self._cert_init_container())
            containers.append(self._orr_container(image=orr_image, image_pull_policy=image_pull_policy))
            containers.append(self._qkc_container(image=qkc_image, image_pull_policy=image_pull_policy))
        if self._observability_enabled():
            volumes.append(
                client.V1Volume(
                    name=self._promtail_config_volume_name(),
                    config_map=client.V1ConfigMapVolumeSource(
                        name=self._promtail_config_map_name()
                    ),
                )
            )
            containers.append(self._promtail_container(image_pull_policy=image_pull_policy))

        deployment = self._build_deployment(
            containers=containers,
            volumes=volumes,
            image_pull_secret=image_pull_secret,
            init_containers=init_containers or None,
        )
        try:
            return self.apps_v1_api.create_namespaced_deployment(
                namespace=self.namespace,
                body=deployment,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(self.apps_v1_api.read_namespaced_deployment, self.name)
            raise


# ─────────────────────────────────────────────────────────────────────
# PodQudittoLink — v3.3: simulador QKD desacoplado del DKMS, un pod
# por enlace bidireccional. Cada pod hostea AMBAS vistas (node_a y
# node_b) en un único proceso `simple_quditto` con config
# ``nodes:[A,B]``. El handler HTTP enruta por ``sae_id`` (ver
# ``code_dkms/src/simple_quditto/server.py::_resolve_link``).
# ─────────────────────────────────────────────────────────────────────


class _LinkChannelSpec:
    """Snapshot inmutable de los parámetros de canal de un enlace QKD."""

    __slots__ = ("distance", "rate_r0", "rate_alpha", "max_buffer_size", "ttl")

    def __init__(
        self,
        *,
        distance: int = 0,
        rate_r0: float = K8S_QUDITTO_DEFAULT_RATE_R0,
        rate_alpha: float = K8S_QUDITTO_DEFAULT_RATE_ALPHA,
        max_buffer_size: int = K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE,
        ttl: int = K8S_QUDITTO_DEFAULT_TTL,
    ) -> None:
        self.distance = int(distance)
        self.rate_r0 = float(rate_r0)
        self.rate_alpha = float(rate_alpha)
        self.max_buffer_size = int(max_buffer_size)
        self.ttl = int(ttl)


def _link_channel_spec_from_kme_entry(kme_entry: Dict[str, Any]) -> _LinkChannelSpec:
    """Extrae los parámetros de canal desde un entry de ``kmes`` del QKC.

    Reutiliza el mismo parseo que el sidecar usaba pre-v3.3 (los nombres
    de los campos no han cambiado; ver ``_extract_quditto_topology`` de
    `PodDKMS` para el contrato).
    """
    channel = kme_entry.get("channel") if isinstance(kme_entry, dict) else None
    if not isinstance(channel, dict):
        return _LinkChannelSpec()
    distance = max(0, _safe_int(channel.get("distance")) or 0)
    max_buffer_size = _safe_positive_int(
        channel.get("max_buffer_size") or channel.get("quditto_max_buffer_size"),
        K8S_QUDITTO_DEFAULT_MAX_BUFFER_SIZE,
    )
    raw_rate_r0 = channel.get("rate_r0")
    if raw_rate_r0 is None:
        raw_rate_r0 = channel.get("quditto_rate_r0")
    raw_rate_alpha = channel.get("rate_alpha")
    if raw_rate_alpha is None:
        raw_rate_alpha = channel.get("quditto_rate_alpha")
    rate_r0 = _safe_positive_float(raw_rate_r0, K8S_QUDITTO_DEFAULT_RATE_R0)
    rate_alpha = _safe_nonnegative_float(raw_rate_alpha, K8S_QUDITTO_DEFAULT_RATE_ALPHA)
    ttl = K8S_QUDITTO_DEFAULT_TTL
    try:
        ttl_candidate = int(channel.get("ttl"))
        if ttl_candidate > 0:
            ttl = ttl_candidate
    except (TypeError, ValueError):
        pass
    return _LinkChannelSpec(
        distance=distance,
        rate_r0=rate_r0,
        rate_alpha=rate_alpha,
        max_buffer_size=max_buffer_size,
        ttl=ttl,
    )


class PodQudittoLink:
    """Deployment + Service + ConfigMap para UN enlace QKD bidireccional.

    Naming canónico: `quditto-link-{min(a,b)}-{max(a,b)}`. Ambos extremos
    del enlace resuelven al mismo DNS:
        http://quditto-link-{a}-{b}.{ns}.svc.cluster.local:5000

    El pod corre `simple_quditto` con config `nodes:[A,B]`, cada uno
    declarando como vecino al otro. Los buffers materializados de los 4
    sentidos (enc_A→B, dec_B←A, enc_B→A, dec_A←B) viven en este mismo
    proceso y el routing HTTP por `sae_id` los enruta al correcto.

    Resources: 300m/384Mi requests, 1500m/1Gi limits — el trabajo per-link
    está acotado a 4 buffers × 2000 keys/s = 8 000 keys/s.
    """

    __TYPE__ = "quditto-link"

    def __init__(
        self,
        id_simulation: str,
        node_a_id: int,
        node_b_id: int,
        *,
        channel: _LinkChannelSpec,
        node_label_prefix: str = "DKMS",
    ) -> None:
        a, b = sorted([int(node_a_id), int(node_b_id)])
        self.node_a_id = a
        self.node_b_id = b
        self.namespace = str(id_simulation)
        self.name = f"{self.__TYPE__}-{a}-{b}"
        self._channel = channel
        self._node_label_prefix = node_label_prefix
        self._config_map_name = self.name  # config + pod + service comparten name

        try:
            config.load_incluster_config()
        except ConfigException:
            config.load_kube_config()
        self.core_v1_api = client.CoreV1Api()
        self.apps_v1_api = client.AppsV1Api()

    # ─── helpers internos ─────────────────────────────────────────────
    def _node_name(self, node_id: int) -> str:
        # v3.4: con K8S_DKMS_RUST_SIDECARS, los DKMS Rust+QKC piden las
        # keys a quditto usando el qkc_id bare ("100001"), no el prefijo
        # legacy "DKMS-100001". Ajustamos el node_name del config para
        # que el match sae_id↔node funcione en ambos paths.
        if K8S_DKMS_RUST_SIDECARS:
            return str(int(node_id))
        return f"{self._node_label_prefix}-{int(node_id)}"

    def _labels(self) -> Dict[str, str]:
        return {
            "app": self.__TYPE__,
            "instance": self.name,
            "link.a": str(self.node_a_id),
            "link.b": str(self.node_b_id),
        }

    def _quditto_config_yaml(self) -> str:
        """Genera el config.yaml con AMBOS extremos del enlace.

        Cada `node` declara al otro como su único vecino. El `role`
        respeta la convención canónica (`originator_for` en
        ``simple_quditto/link.py``): el nodo con menor nombre lex es "A"
        (initiator), el otro "B" (responder). Como `_node_name` usa el
        mismo prefijo y los ids ya están sorted, el nombre menor lex es
        siempre `node_a_id`.
        """
        name_a = self._node_name(self.node_a_id)
        name_b = self._node_name(self.node_b_id)
        common = {
            "ttl": self._channel.ttl,
            "max_buffer_size": self._channel.max_buffer_size,
            "distance": self._channel.distance,
            "rate_r0": self._channel.rate_r0,
            "rate_alpha": self._channel.rate_alpha,
        }
        payload = {
            "quditto_version": "3.3",
            "nodes": [
                {
                    "node_name": name_a,
                    "neighbour_nodes": [{
                        "name": name_b,
                        "role": "initiator",
                        **common,
                    }],
                },
                {
                    "node_name": name_b,
                    "neighbour_nodes": [{
                        "name": name_a,
                        "role": "responder",
                        **common,
                    }],
                },
            ],
        }
        return json.dumps(payload, ensure_ascii=True, indent=2)

    def _read_namespaced(self, read_fn, name: str):
        try:
            return read_fn(name=name, namespace=self.namespace)
        except ApiException as e:
            if e.status == 404:
                return None
            raise

    # ─── API pública: configmap, pod, service ─────────────────────────
    def crear_config_map(self):
        existing = self._read_namespaced(
            self.core_v1_api.read_namespaced_config_map, self._config_map_name,
        )
        cm = client.V1ConfigMap(
            metadata=client.V1ObjectMeta(
                name=self._config_map_name,
                labels=self._labels(),
            ),
            data={"config.yaml": self._quditto_config_yaml()},
        )
        if existing is not None:
            if existing.metadata is not None:
                rv = getattr(existing.metadata, "resource_version", None)
                if rv:
                    if cm.metadata is None:
                        cm.metadata = client.V1ObjectMeta(name=self._config_map_name)
                    cm.metadata.resource_version = rv
            return self.core_v1_api.replace_namespaced_config_map(
                name=self._config_map_name,
                namespace=self.namespace,
                body=cm,
            )
        try:
            return self.core_v1_api.create_namespaced_config_map(
                namespace=self.namespace, body=cm,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(
                    self.core_v1_api.read_namespaced_config_map,
                    self._config_map_name,
                )
            raise

    def create_pod(
        self,
        env_vars: dict | None = None,
        image: str | None = None,
        ports: list | None = None,
        image_pull_secret: str | None = None,
        image_pull_policy: str | None = None,
        **_ignored,
    ):
        existing = self._read_namespaced(
            self.apps_v1_api.read_namespaced_deployment, self.name,
        )
        if existing:
            return existing

        if image is None:
            image = K8S_QUDITTO_IMAGE
        if not image_pull_policy:
            image_pull_policy = K8S_IMAGE_PULL_POLICY
        if image_pull_secret is None and DOCKER_HUB_TOKEN:
            image_pull_secret = DOCKER_HUB_SECRET_NAME

        # v3.4: dos paths según el image:
        #   - `pablopio/quditto:*` (Rust): CLI propia, sin configmap, sin
        #     certs (HTTP plano dentro del namespace).
        #   - cualquier otro (Python `simple_quditto`): config.yaml + certs
        #     auto-generados + uvicorn workers.
        is_rust_quditto = "/quditto" in image and "/simple-quditto" not in image
        if is_rust_quditto:
            command = [
                "/usr/local/bin/quditto",
                "--listen", f"0.0.0.0:{K8S_QUDITTO_PORT}",
                "--r0", str(self._channel.rate_r0),
                "--alpha", str(self._channel.rate_alpha),
                "--distance", str(float(self._channel.distance)),
                "--max-buffer", str(int(self._channel.max_buffer_size)),
                "--key-size-bits", "256",
            ]
            env_list = [client.V1EnvVar(name="RUST_LOG", value="info")]
        else:
            command = [
                "python", "-m", "simple_quditto",
                "--config", "/app/config/config.yaml",
                "--port", str(K8S_QUDITTO_PORT),
                "--cert", "/app/certs/server.crt",
                "--key", "/app/certs/server.key",
                "--workers", "1",
                "--insecure",
            ]
            env_list = [
                client.V1EnvVar(name="VERBOSE", value=K8S_QUDITTO_VERBOSE),
                client.V1EnvVar(name="QUDITTO_WORKERS", value="1"),
            ]
        if env_vars:
            for k, v in env_vars.items():
                env_list.append(client.V1EnvVar(name=str(k), value=str(v)))

        container = client.V1Container(
            name="quditto",
            image=image,
            image_pull_policy=image_pull_policy,
            command=command,
            env=env_list,
            ports=[
                client.V1ContainerPort(container_port=K8S_QUDITTO_PORT),
                client.V1ContainerPort(container_port=K8S_QUDITTO_METRICS_PORT),
            ],
            resources=client.V1ResourceRequirements(
                # Subido cpu 300m→500m para empujar a Karpenter fuera de
                # nodos c5a.large (2 CPU). Con 14 quditto-links × 500m +
                # 10 DKMSs × 750m = 14.5 CPU mínimos: Karpenter elige
                # ≥c5a.xlarge (4 CPU) o c5a.4xlarge (16 CPU).
                requests={"cpu": "500m", "memory": "512Mi"},
                limits={"cpu": "1500m", "memory": "1Gi"},
            ),
            startup_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_QUDITTO_PORT),
                period_seconds=2,
                timeout_seconds=2,
                failure_threshold=90,
            ),
            readiness_probe=client.V1Probe(
                tcp_socket=client.V1TCPSocketAction(port=K8S_QUDITTO_PORT),
                period_seconds=5,
                timeout_seconds=2,
                failure_threshold=6,
            ),
            volume_mounts=(
                [
                    client.V1VolumeMount(name="logs", mount_path="/app/logs", read_only=False),
                ]
                if is_rust_quditto
                else [
                    client.V1VolumeMount(
                        name="config", mount_path="/app/config", read_only=True,
                    ),
                    client.V1VolumeMount(
                        name="certs", mount_path="/app/certs", read_only=False,
                    ),
                    client.V1VolumeMount(
                        name="logs", mount_path="/app/logs", read_only=False,
                    ),
                ]
            ),
        )

        if is_rust_quditto:
            volumes = [
                client.V1Volume(name="logs", empty_dir=client.V1EmptyDirVolumeSource()),
            ]
        else:
            volumes = [
                client.V1Volume(
                    name="config",
                    config_map=client.V1ConfigMapVolumeSource(name=self._config_map_name),
                ),
                client.V1Volume(name="certs", empty_dir=client.V1EmptyDirVolumeSource()),
                client.V1Volume(name="logs", empty_dir=client.V1EmptyDirVolumeSource()),
            ]

        labels = self._labels()
        pod_template = client.V1PodTemplateSpec(
            metadata=client.V1ObjectMeta(labels=labels),
            spec=client.V1PodSpec(
                containers=[container],
                volumes=volumes,
                restart_policy="Always",
                image_pull_secrets=(
                    [client.V1LocalObjectReference(name=image_pull_secret)]
                    if image_pull_secret else None
                ),
            ),
        )
        deployment = client.V1Deployment(
            metadata=client.V1ObjectMeta(name=self.name, labels=labels),
            spec=client.V1DeploymentSpec(
                replicas=1,
                selector=client.V1LabelSelector(match_labels=labels),
                template=pod_template,
                strategy=client.V1DeploymentStrategy(
                    type="RollingUpdate",
                    rolling_update=client.V1RollingUpdateDeployment(
                        max_unavailable=0,
                        max_surge=1,
                    ),
                ),
            ),
        )
        try:
            return self.apps_v1_api.create_namespaced_deployment(
                namespace=self.namespace, body=deployment,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(
                    self.apps_v1_api.read_namespaced_deployment, self.name,
                )
            raise

    def create_service(self):
        labels = self._labels()
        existing = self._read_namespaced(
            self.core_v1_api.read_namespaced_service, self.name,
        )
        if existing is not None:
            return existing
        svc = client.V1Service(
            metadata=client.V1ObjectMeta(name=self.name, labels=labels),
            spec=client.V1ServiceSpec(
                type="ClusterIP",
                selector=labels,
                ports=[
                    client.V1ServicePort(
                        name="http",
                        port=K8S_QUDITTO_PORT,
                        target_port=K8S_QUDITTO_PORT,
                        protocol="TCP",
                    ),
                    client.V1ServicePort(
                        name="metrics",
                        port=K8S_QUDITTO_METRICS_PORT,
                        target_port=K8S_QUDITTO_METRICS_PORT,
                        protocol="TCP",
                    ),
                ],
            ),
        )
        try:
            return self.core_v1_api.create_namespaced_service(
                namespace=self.namespace, body=svc,
            )
        except ApiException as e:
            if e.status == 409:
                return self._read_namespaced(
                    self.core_v1_api.read_namespaced_service, self.name,
                )
            raise


def build_quditto_link_pods_from_dkms_models(
    id_simulation: str,
    dkms_models: list,
    *,
    node_label_prefix: str = "DKMS",
) -> list:
    """Itera los ModelDKMS y construye 1 `PodQudittoLink` por edge único.

    Cada DKMS pod tiene en su `model_dkms.qkc.kmes` la lista de KMEs (1
    por enlace QKD). Cada KME aparece DOS veces en el cluster (una en
    cada extremo), por eso dedupe por canonical (min, max).

    Reusa `_link_channel_spec_from_kme_entry` para extraer distance/rate
    desde cualquiera de los dos extremos (deberían coincidir).

    Devuelve lista de `PodQudittoLink` (uno por enlace QKD).
    """
    seen: Dict[tuple, _LinkChannelSpec] = {}
    for model_dkms in dkms_models:
        # ModelDKMS → orr → qkc → kmes (mismo path que usa PodDKMS
        # `_extract_qkc_payload` en pods.py:2766-2773).
        orr = getattr(model_dkms, "orr", None)
        qkc = getattr(orr, "qkc", None) if orr is not None else None
        if qkc is None:
            continue
        # `qkc` puede ser objeto pydantic o dict — extraemos kmes.
        kmes = None
        if hasattr(qkc, "kmes"):
            kmes = getattr(qkc, "kmes")
        elif isinstance(qkc, dict):
            kmes = qkc.get("kmes")
        if not kmes:
            continue
        for kme in kmes:
            kme_dict = (
                kme if isinstance(kme, dict)
                else kme.model_dump() if hasattr(kme, "model_dump")
                else None
            )
            if not kme_dict:
                continue
            if not _is_qkd_link(kme_dict):
                continue
            try:
                local_id = int(kme_dict.get("local_qkc_id"))
                neighbor_id = int(kme_dict.get("neighbor_qkc_id"))
            except (TypeError, ValueError):
                continue
            key = tuple(sorted([local_id, neighbor_id]))
            if key in seen:
                continue
            spec = _link_channel_spec_from_kme_entry(kme_dict)
            seen[key] = spec

    return [
        PodQudittoLink(
            id_simulation=id_simulation,
            node_a_id=a,
            node_b_id=b,
            channel=spec,
            node_label_prefix=node_label_prefix,
        )
        for (a, b), spec in sorted(seen.items())
    ]


if __name__ == "__main__":
    import os

    image_sdn = os.getenv("SDN_IMAGE")
    image_dkms = os.getenv("DKMS_IMAGE")


    image_pull_secret = os.getenv("K8S_IMAGE_PULL_SECRET")
    image_pull_policy = os.getenv("K8S_IMAGE_PULL_POLICY")


    path_config = '/home/pablopio/Documentos/trabajo_atlantic/dkms/code_dkms/config_files/DKMS'

    list_configs = os.listdir(path_config)
    list_configs.sort()

    dkms_pods = []
    for dkms_config in list_configs:
        
        model_dkms = ModelDKMS.model_from_json_file(os.path.join(path_config,dkms_config))
        pod_dkms = PodDKMS(id_simulation=1,model_dkms=model_dkms)
        pod_dkms.create_namespace()
        pod_dkms.create_pod(
            image=image_dkms,
            image_pull_secret=image_pull_secret,
            image_pull_policy=image_pull_policy,
        )
        pod_dkms.create_service()
        dkms_pods.append(pod_dkms)


    model_sdn = ModelSDN.model_from_json_file('/home/pablopio/Documentos/trabajo_atlantic/dkms/code_dkms/config_files/SDN/1.json')
    pod_sdn = PodSDN(id_simulation=1,model_sdn=model_sdn)
    pod_sdn.create_namespace()
    pod_sdn.create_pod(
        image=image_sdn,
        image_pull_secret=image_pull_secret,
        image_pull_policy=image_pull_policy,
    )
    pod_sdn.create_service()
    pod_sdn.create_simulation_ingress(dkms_pods=dkms_pods, sdn_pod=pod_sdn)
    
