from __future__ import annotations

import base64
import json
import os
import socket
import sys
import time
from pathlib import Path
from typing import Any, Dict, Tuple
from urllib import error as urllib_error
from urllib import request as urllib_request
from urllib.parse import urlparse

# Ensure the project src/ is on sys.path when running as a script
ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.append(str(ROOT))

from models import (
    KMEConfig,
    ModelDKMS,
    ModelORR,
    ModelQKC,
    ModelSDN,
    ModelSimulation,
    SimulationStatus,
)
from persistence import UnitOfWork, build_uow_from_env
try:
    from k8s.runtime_ca import sync_runtime_ca_to_simulation_namespace
except ModuleNotFoundError:  # pragma: no cover - fallback for script execution
    from runtime_ca import sync_runtime_ca_to_simulation_namespace

from dotenv import load_dotenv

load_dotenv()

ORCH_SDN_SYNC_TIMEOUT_SECONDS = max(
    1.0,
    float(os.getenv("ORCH_SDN_SYNC_TIMEOUT_SECONDS", "5")),
)
ORCH_SDN_RECONCILE_READY_TIMEOUT_SECONDS = max(
    1.0,
    float(os.getenv("ORCH_SDN_RECONCILE_READY_TIMEOUT_SECONDS", "60")),
)
ORCH_SDN_RECONCILE_RETRY_INTERVAL_SECONDS = max(
    0.2,
    float(os.getenv("ORCH_SDN_RECONCILE_RETRY_INTERVAL_SECONDS", "2")),
)


class Orchestator:
    """
    El Orchestator es el encargado de manejar los:
        - Simulaciones:
            Lista de PodDKMS
            El PodSDN
        - Bases de datos
    """

    def __init__(
        self,
        url: str | None = None,
        uow: UnitOfWork | None = None,
        image_sdn: str | None = None,
        image_dkms: str | None = None,
        image_quditto: str | None = None,
        image_pull_secret: str | None = None,
        image_pull_policy: str | None = None,
    ) -> None:
        self.url = url
        self.uow = uow or build_uow_from_env()
        self.pods_dkms: Dict[Tuple[str, int], pods.PodDKMS] = {}
        self.pods_sdn: Dict[str, pods.PodSDN] = {}
        self.pods_observability: Dict[str, pods.PodObservability] = {}
        self.image_sdn = image_sdn or os.getenv("SDN_IMAGE")
        self.image_dkms = image_dkms or os.getenv("DKMS_IMAGE")
        self.image_quditto = image_quditto or os.getenv("QUDITTO_IMAGE")
        self.image_pull_secret = image_pull_secret or os.getenv("K8S_IMAGE_PULL_SECRET")
        self.image_pull_policy = image_pull_policy or os.getenv("K8S_IMAGE_PULL_POLICY")
        self.namespace_delete_timeout_seconds = max(
            5.0,
            float(os.getenv("K8S_NAMESPACE_DELETE_TIMEOUT_SECONDS", "240")),
        )
        self.namespace_delete_poll_interval_seconds = max(
            0.2,
            float(os.getenv("K8S_NAMESPACE_DELETE_POLL_INTERVAL_SECONDS", "2")),
        )

    @staticmethod
    def _normalize_simulation_id(id_simulation: int | str) -> tuple[int, str]:
        try:
            simulation_id = int(id_simulation)
        except (TypeError, ValueError) as exc:
            raise ValueError("id_simulation debe ser un entero") from exc
        return simulation_id, str(simulation_id)

    @staticmethod
    def _normalize_user_id(user_id: int | str) -> int:
        try:
            return int(user_id)
        except (TypeError, ValueError) as exc:
            raise ValueError("user_id debe ser un entero") from exc

    @staticmethod
    def _normalize_dkms_id(id_dkms: int | str) -> int:
        try:
            dkms_id = int(id_dkms)
        except (TypeError, ValueError) as exc:
            raise ValueError("id_dkms debe ser un entero") from exc
        if dkms_id <= 0:
            raise ValueError("id_dkms debe ser mayor que cero")
        return dkms_id

    def _get_simulation(self, simulation_id: int) -> ModelSimulation:
        sim = self.uow.repos.simulations.get(simulation_id)
        if sim is None:
            raise ValueError(f"Simulacion {simulation_id} no encontrada")
        return sim

    @staticmethod
    def _ensure_simulation_owner(simulation: ModelSimulation, user_id: int) -> None:
        if simulation.id_user != user_id:
            raise ValueError(
                f"Simulacion {simulation.id} no pertenece al usuario {user_id}"
            )

    @staticmethod
    def _resolve_dkms_id(simulation: ModelSimulation, requested_id: int) -> int:
        """Devuelve el `dkms.id` real (BD) a partir del id que envía el
        cliente. La UI del editor usa el `node_id` del editor_topology_json
        (1..N), mientras que la BD asigna IDs autoincrementales (e.g.
        151..160). Como `_build_model_simulation_from_web` crea los DKMS
        ordenados por `node_id`, podemos recuperar la correspondencia
        emparejando `sorted(list_dkms, key=id)` con `sorted(nodes, key=node_id)`.

        Resolución:
        1. Si `requested_id` matchea directamente un `dkms.id` real → devuelve.
        2. Si matchea un `node_id` del editor → traduce vía el orden.
        3. Si no, devuelve `requested_id` (el llamador genera el ValueError).
        """
        list_dkms = list(getattr(simulation, "list_dkms", []) or [])
        if any(int(getattr(d, "id", 0) or 0) == requested_id for d in list_dkms):
            return requested_id
        topology_raw = getattr(simulation, "editor_topology_json", None)
        if not topology_raw:
            return requested_id
        try:
            topology = json.loads(topology_raw)
        except (TypeError, ValueError):
            return requested_id
        # editor_topology_json puede venir en dos formatos:
        # - BD interna (WebSimulationUpsertRequest): {"nodes": [...], ...}
        # - JSON exportado/importado: {"extensions": {"editor": {"nodes": [...]}}}
        # Y los nodos pueden o no tener `node_type` (la BD no lo guarda;
        # todos los nodos web son DKMS por ahora).
        nodes = topology.get("nodes") or (
            topology.get("extensions", {}).get("editor", {}).get("nodes", [])
        )
        if not isinstance(nodes, list):
            return requested_id
        dkms_nodes = [
            n for n in nodes
            if isinstance(n, dict) and n.get("node_type", "DKMS") == "DKMS"
        ]
        try:
            sorted_nodes = sorted(dkms_nodes, key=lambda n: int(n.get("node_id", 0)))
        except (TypeError, ValueError):
            return requested_id
        sorted_dkms = sorted(list_dkms, key=lambda d: int(getattr(d, "id", 0) or 0))
        for node, dkms in zip(sorted_nodes, sorted_dkms):
            try:
                if int(node.get("node_id", 0)) == requested_id:
                    return int(getattr(dkms, "id", 0) or 0)
            except (TypeError, ValueError):
                continue
        return requested_id

    def _purge_loadtest_sae_residue(self, simulation_id: int) -> int:
        """Borra de la DB SAEs loadtest (prefijo ``lt-``) huérfanas.

        Los clientes de ``/code_dkms/src/loadtest/runner.py`` generan IDs
        ``lt-<instance>-<test>-<...>`` y normalmente llaman DELETE al
        terminar. Bajo SDN saturado ese DELETE suele devolver 503 y la
        fila queda en la DB. Al relanzar la sim se re-registra en el
        SDN, saturándolo con estado huérfano. Este método se ejecuta al
        inicio de ``run_simulation`` para prevenir la acumulación.
        Devuelve el número de filas borradas.
        """
        try:
            from sqlalchemy import text  # lazy import to avoid cycles
        except ImportError:
            return 0
        sim_pk = int(simulation_id)
        try:
            session_factory = getattr(self.uow, "_session_factory", None)
            if session_factory is None:
                return 0
            with session_factory() as session:
                result = session.execute(
                    text(
                        "DELETE FROM sae "
                        "WHERE simulation_id = :sim_id AND sae_id LIKE 'lt-%%'"
                    ),
                    {"sim_id": sim_pk},
                )
                deleted = int(result.rowcount or 0)
                if deleted > 0:
                    session.commit()
                    print(
                        f"run_simulation: purged {deleted} loadtest SAEs "
                        f"(sim={sim_pk})"
                    )
                return deleted
        except Exception as exc:  # noqa: BLE001
            print(f"_purge_loadtest_sae_residue error: {exc}")
            return 0

    def _list_simulations_from_memory_repo(self, user_id: int) -> list[ModelSimulation]:
        repo = self.uow.repos.simulations
        collection_getter = getattr(repo, "_collection", None)
        if not callable(collection_getter):
            raise RuntimeError(
                "El backend de persistencia no soporta listar simulaciones por usuario"
            )

        collection = collection_getter()
        models = [
            model.model_copy(deep=True)
            for model in collection.values()
            if model.id_user == user_id
        ]
        models.sort(key=lambda sim: sim.id or 0)
        return models

    def _deploy_pod(
        self,
        pod: "pods.Pod",
        image: str | None = None,
        image_pull_secret: str | None = None,
        **pod_kwargs,
    ) -> None:
        pod.crear_config_map()
        pod.create_pod(
            image=image,
            image_pull_secret=image_pull_secret,
            image_pull_policy=self.image_pull_policy,
            **pod_kwargs,
        )
        pod.create_service()

    def _forget_simulation_pods(self, simulation_key: str) -> None:
        self.pods_sdn.pop(simulation_key, None)
        self.pods_observability.pop(simulation_key, None)
        keys_to_delete = [key for key in self.pods_dkms if key[0] == simulation_key]
        for key in keys_to_delete:
            self.pods_dkms.pop(key, None)

    @staticmethod
    def _load_pods_module():
        try:
            from k8s import pods as pods_module
        except ModuleNotFoundError:
            import pods as pods_module
        return pods_module

    @staticmethod
    def _build_sdn_runtime_topology_env(
        simulation: ModelSimulation,
        *,
        pods_module: Any,
    ) -> dict[str, str]:
        env_payload: dict[str, str] = {
            "SDN_ENABLE_DKMS_METRICS_LINK_STATE_SYNC": "true",
            "SDN_ENABLE_QKC_LINK_CAPACITY_SYNC": "true",
            # MCF es la autoridad del plano de control bajo la nueva
            # arquitectura; los DKMS tienen RSVP-TE heartbeat desactivado
            # (DKMS_LSP_HEARTBEAT_ENABLED=0). Si el zombie sweeper sigue
            # activo, purga los LSPs por falta de heartbeat y el DKMS
            # pierde su pacer → stall del plano de datos. Se puede
            # reactivar si se quiere volver al modelo RSVP-TE puro.
            "SDN_LSP_ZOMBIE_SWEEPER_ENABLED": os.getenv(
                "SDN_LSP_ZOMBIE_SWEEPER_ENABLED", "0"
            ),
            # Heartbeat/sweep intervals: irrelevantes con sweeper off,
            # pero los mantenemos para compat si se reactiva.
        }
        # Rust SDN: bind the gRPC listener to the Service port (legacy
        # model.host.port, e.g. 3000), seed required SDN__* env vars, and
        # point the topology_dir to the ConfigMap mount path created by
        # ``pods.PodSDN.ensure_rust_topology_configmap``.
        if getattr(pods_module, "K8S_DKMS_RUST_SIDECARS", False):
            sdn_host = getattr(simulation.sdn, "host", None) if simulation.sdn else None
            sdn_port = getattr(sdn_host, "port", None) if sdn_host else None
            try:
                sdn_port_int = int(sdn_port) if sdn_port is not None else 3000
            except (TypeError, ValueError):
                sdn_port_int = 3000
            sim_id = simulation.id if simulation.id is not None else 0
            # DKMS Rust derives the SDN HTTP URL as ``grpc_port + 2`` (see
            # ``dkms/src/main.rs::derive_sdn_http_url``). To match that
            # convention the SDN HTTP must bind on ``sdn_port_int + 2`` and
            # the Service must expose both ports — see
            # ``PodSDN.expose_rust_http_port``.
            env_payload.update({
                "RUST_LOG": "info",
                "CONFIG_DIR": "/app/config/sdn",
                "SDN__node_id": f"sdn-sim-{int(sim_id)}",
                "SDN__grpc_addr": f"0.0.0.0:{sdn_port_int}",
                "SDN__http_addr": f"0.0.0.0:{sdn_port_int + 2}",
                "SDN__metrics_addr": "0.0.0.0:9102",
                "SDN__topology_dir": "/app/topology",
            })
        canonicalize = getattr(pods_module, "_canonicalize_runtime_dkms_payload", None)
        if not callable(canonicalize):
            return env_payload

        runtime_dkms_payloads: list[dict[str, Any]] = []
        for dkms in simulation.list_dkms or []:
            model_dump = getattr(dkms, "model_dump", None)
            if not callable(model_dump):
                continue
            try:
                raw_payload = model_dump(mode="json", exclude_none=True)
                canonical_payload = canonicalize(raw_payload)
            except Exception:
                continue
            if isinstance(canonical_payload, dict) and canonical_payload:
                runtime_dkms_payloads.append(canonical_payload)

        if not runtime_dkms_payloads:
            return env_payload

        # 2026-05-20: skip injecting SDN_TOPOLOGY_JSON_B64 if the
        # encoded payload would push env+args past ARG_MAX (~128 KB
        # on Linux). The Rust SDN reads its topology from
        # /app/topology (ConfigMap mount) at boot, so this env is
        # legacy. For N>=40 it always overflowed and triggered
        # "exec /usr/local/bin/sdn: argument list too long".
        encoded_payload = base64.b64encode(
            json.dumps(
                {"dkms": runtime_dkms_payloads},
                ensure_ascii=True,
                separators=(",", ":"),
            ).encode("utf-8")
        ).decode("ascii")
        ARG_MAX_SOFT_LIMIT = 96 * 1024  # leave headroom for other env vars
        if len(encoded_payload) <= ARG_MAX_SOFT_LIMIT:
            env_payload["SDN_TOPOLOGY_JSON_B64"] = encoded_payload
        else:
            import logging
            logging.getLogger(__name__).warning(
                "SDN_TOPOLOGY_JSON_B64 omitted (size %d > limit %d); SDN loads from /app/topology ConfigMap",
                len(encoded_payload),
                ARG_MAX_SOFT_LIMIT,
            )
        return env_payload

    def _wait_namespace_deleted(self, pod_sdn: "pods.PodSDN") -> bool:
        deadline = time.monotonic() + self.namespace_delete_timeout_seconds
        while time.monotonic() < deadline:
            if pod_sdn._read_namespace() is None:
                return True
            time.sleep(self.namespace_delete_poll_interval_seconds)
        return pod_sdn._read_namespace() is None

    @staticmethod
    def _is_missing_model_id_error(exc: Exception) -> bool:
        return "id_host ni id" in str(exc).lower()

    def _build_namespace_only_pod(self, simulation_key: str, pods_module):
        api_exception = getattr(pods_module, "ApiException", Exception)
        config_exception = getattr(pods_module, "ConfigException", Exception)
        config_module = getattr(pods_module, "config", None)
        client_module = getattr(pods_module, "client", None)

        if config_module is None or client_module is None:
            raise RuntimeError("El modulo pods no expone cliente/config de Kubernetes")

        try:
            config_module.load_incluster_config()
        except config_exception:
            config_module.load_kube_config()

        core_v1_api = client_module.CoreV1Api()

        class _NamespaceOnlyPod:
            def __init__(self, namespace: str):
                self.namespace = str(namespace)
                self.core_v1_api = core_v1_api

            def eliminar_namespace(self):
                try:
                    return self.core_v1_api.delete_namespace(name=self.namespace)
                except api_exception as api_exc:
                    if getattr(api_exc, "status", None) == 404:
                        return None
                    raise

            def _read_namespace(self):
                try:
                    return self.core_v1_api.read_namespace(name=self.namespace)
                except api_exception as api_exc:
                    if getattr(api_exc, "status", None) == 404:
                        return None
                    raise

        return _NamespaceOnlyPod(simulation_key)

    @staticmethod
    def _is_namespace_terminating(namespace_obj: Any) -> bool:
        if namespace_obj is None:
            return False
        metadata = getattr(namespace_obj, "metadata", None)
        status = getattr(namespace_obj, "status", None)
        deletion_timestamp = getattr(metadata, "deletion_timestamp", None)
        phase = str(getattr(status, "phase", "") or "").strip().lower()
        return deletion_timestamp is not None or phase == "terminating"

    def _wait_namespace_ready_for_run(self, pod_sdn: "pods.PodSDN", simulation_key: str) -> None:
        deadline = time.monotonic() + self.namespace_delete_timeout_seconds
        while True:
            namespace_obj = pod_sdn._read_namespace()
            if namespace_obj is None:
                # Namespace does not exist: create and continue.
                pod_sdn.create_namespace()
                return
            if not self._is_namespace_terminating(namespace_obj):
                # Namespace exists and is active.
                pod_sdn.create_namespace()
                return
            if time.monotonic() >= deadline:
                raise ValueError(
                    "El namespace de la simulacion sigue en estado Terminating. "
                    f"Espera a que finalice el borrado de {simulation_key} y vuelve a intentarlo."
                )
            time.sleep(self.namespace_delete_poll_interval_seconds)

    def _ensure_runtime_ca_secret(self, *, simulation_id: int, simulation_namespace: str) -> None:
        sync_runtime_ca_to_simulation_namespace(
            simulation_id=int(simulation_id),
            simulation_namespace=str(simulation_namespace),
        )

    @staticmethod
    def _simulation_sdn_service_base_url(
        *,
        simulation: ModelSimulation,
        simulation_key: str,
    ) -> str:
        sdn_model = getattr(simulation, "sdn", None)
        if sdn_model is None:
            raise ValueError("La simulacion no tiene SDN asociado para sincronizar SAE")

        sdn_host = getattr(sdn_model, "host", None)
        if sdn_host is None or getattr(sdn_host, "port", None) is None:
            raise ValueError("La simulacion no tiene host/port SDN para sincronizar SAE")

        sdn_suffix = getattr(sdn_model, "id_host", None)
        if sdn_suffix is None:
            sdn_suffix = getattr(sdn_model, "id", None)
        if sdn_suffix is None:
            raise ValueError("No se pudo resolver el nombre de servicio SDN")

        try:
            sdn_port = int(sdn_host.port)
        except (TypeError, ValueError) as exc:
            raise ValueError(f"Puerto SDN invalido para sincronizar SAE: {sdn_host.port}") from exc

        return f"http://sdn-{int(sdn_suffix)}.{simulation_key}.svc.cluster.local:{sdn_port}"

    @staticmethod
    def _sdn_http_json(
        *,
        method: str,
        url: str,
        payload: Dict[str, Any] | None = None,
    ) -> tuple[int, str]:
        parsed_url = urlparse(url)
        if parsed_url.scheme not in {"http", "https"} or not parsed_url.netloc:
            raise ValueError(f"URL SDN invalida: {url}")

        body: bytes | None = None
        headers: dict[str, str] = {}
        if payload is not None:
            body = json.dumps(payload, ensure_ascii=True).encode("utf-8")
            headers["Content-Type"] = "application/json"

        request = urllib_request.Request(
            url=url,
            data=body,
            headers=headers,
            method=method,
        )
        try:
            # URL validada arriba: solo se permite http/https con netloc explícito.
            with urllib_request.urlopen(request, timeout=ORCH_SDN_SYNC_TIMEOUT_SECONDS) as response:  # nosec B310
                raw_body = response.read().decode("utf-8", errors="replace")
                return int(response.status), raw_body
        except urllib_error.HTTPError as exc:
            raw_body = exc.read().decode("utf-8", errors="replace")
            return int(exc.code), raw_body
        except (urllib_error.URLError, TimeoutError, socket.timeout) as exc:
            raise ValueError(f"SDN no accesible: {exc}") from exc

    def _sdn_upsert_sae_binding(self, *, sdn_base_url: str, sae_id: str, dkms_id: int) -> None:
        post_status, post_body = self._sdn_http_json(
            method="POST",
            url=f"{sdn_base_url}/sae/",
            payload={"id": str(sae_id), "dkms_id": str(int(dkms_id))},
        )
        if post_status in {200, 201}:
            return
        if post_status == 409:
            put_status, put_body = self._sdn_http_json(
                method="PUT",
                url=f"{sdn_base_url}/sae/{sae_id}",
                payload={"dkms_id": str(int(dkms_id))},
            )
            if put_status == 200:
                return
            raise ValueError(
                f"SDN rechazo update SAE {sae_id} (HTTP {put_status}): {put_body or '<empty>'}"
            )
        raise ValueError(
            f"SDN rechazo create SAE {sae_id} (HTTP {post_status}): {post_body or '<empty>'}"
        )

    def _reconcile_sae_bindings_to_sdn(
        self,
        *,
        simulation_id: int,
        simulation_key: str,
        simulation: ModelSimulation,
    ) -> None:
        session = getattr(self.uow, "_session", None)
        if session is None:
            return

        from persistence.sqlalchemy.data import DKMS as DKMSEntity
        from persistence.sqlalchemy.data import Host as HostEntity
        from persistence.sqlalchemy.data import SAE as SAEEntity

        sdn_base_url = self._simulation_sdn_service_base_url(
            simulation=simulation,
            simulation_key=simulation_key,
        )

        sae_rows = (
            session.query(SAEEntity)
            .filter(SAEEntity.simulation_id == int(simulation_id))
            .order_by(SAEEntity.id.asc())
            .all()
        )
        dkms_rows = (
            session.query(DKMSEntity.id, HostEntity.ip, HostEntity.port)
            .join(HostEntity, DKMSEntity.id_host == HostEntity.id)
            .filter(HostEntity.id_simulation == int(simulation_id))
            .all()
        )

        def _selector_from_host(ip_value: Any, port_value: Any, fallback_id: int) -> int:
            try:
                port = int(port_value)
            except (TypeError, ValueError):
                port = 0
            if port > 4000:
                return int(port - 4000)

            host_ip = str(ip_value or "").strip()
            octets = host_ip.split(".")
            if len(octets) == 4:
                try:
                    a, b, c, d = (int(item) for item in octets)
                except ValueError:
                    d = -1
                    a = b = c = -1
                if a == 127 and b == 0 and c == 0 and d > 100:
                    return int(d - 100)

            return int(fallback_id)

        sdn_selector_by_dkms_id: dict[int, int] = {}
        for dkms_db_id, host_ip, host_port in dkms_rows:
            if dkms_db_id is None:
                continue
            try:
                parsed_dkms_id = int(dkms_db_id)
            except (TypeError, ValueError):
                continue
            sdn_selector_by_dkms_id[parsed_dkms_id] = _selector_from_host(
                host_ip,
                host_port,
                parsed_dkms_id,
            )

        deadline = time.monotonic() + ORCH_SDN_RECONCILE_READY_TIMEOUT_SECONDS
        last_unreachable_error: ValueError | None = None

        while True:
            try:
                for sae in sae_rows:
                    sae_id = str(getattr(sae, "sae_id", "") or "").strip()
                    dkms_id = getattr(sae, "dkms_id", None)
                    if not sae_id or dkms_id is None:
                        continue
                    try:
                        resolved_dkms_id = int(dkms_id)
                    except (TypeError, ValueError):
                        continue
                    sdn_selector = sdn_selector_by_dkms_id.get(
                        resolved_dkms_id,
                        resolved_dkms_id,
                    )
                    self._sdn_upsert_sae_binding(
                        sdn_base_url=sdn_base_url,
                        sae_id=sae_id,
                        dkms_id=sdn_selector,
                    )
                return
            except ValueError as exc:
                if "SDN no accesible" not in str(exc):
                    raise
                last_unreachable_error = exc
                if time.monotonic() >= deadline:
                    raise last_unreachable_error
                time.sleep(ORCH_SDN_RECONCILE_RETRY_INTERVAL_SECONDS)

    @staticmethod
    def _entity_cache_key(model: object) -> str:
        model_id = getattr(model, "id", None)
        if model_id is not None:
            return f"id:{model_id}"
        host = getattr(model, "host", None)
        if host is not None:
            host_ip = getattr(host, "ip", None)
            host_port = getattr(host, "port", None)
            if host_ip is not None and host_port is not None:
                return f"host:{host_ip}:{host_port}"
        return f"obj:{id(model)}"

    @staticmethod
    def _bind_host_to_simulation(model, simulation_id: int):
        host = getattr(model, "host", None)
        if host is None:
            return model
        updated_host = host.model_copy(update={"id_simulation": simulation_id})
        updates = {"host": updated_host}
        id_host = getattr(model, "id_host", None)
        if id_host is None and updated_host.id is not None:
            updates["id_host"] = updated_host.id
        return model.model_copy(update=updates)

    def create_simulation(self, simulation: ModelSimulation) -> ModelSimulation:
        """
        Persiste en base de datos toda la configuracion de una simulacion.
        """
        simulation_payload = simulation.model_copy(deep=True)

        with self.uow:
            if self.uow.repos.users.get(simulation_payload.id_user) is None:
                raise ValueError(f"Usuario {simulation_payload.id_user} no encontrado")

            # Bug fix (2026-05-20): purge orphan KMEs left behind by
            # previous sims. session.merge() on a QKC with kmes=[]
            # propagates NULL into the local_qkc_id of any orphan KME,
            # violating the NOT NULL constraint. An orphan KME is one
            # whose local_qkc_id no longer points at any row in `qkc`
            # (the QKC was deleted when its sim was torn down, but the
            # KME row stayed alive due to missing cascade). Wipe them up
            # front so the merge has no leftover rows to corrupt.
            session_for_cleanup = getattr(self.uow, "_session", None)
            if session_for_cleanup is not None:
                from sqlalchemy import text as _sql_text
                purged = session_for_cleanup.execute(
                    _sql_text(
                        "DELETE FROM kme "
                        "WHERE local_qkc_id NOT IN (SELECT id FROM qkc) "
                        "OR neighbor_qkc_id NOT IN (SELECT id FROM qkc)"
                    )
                ).rowcount
                if purged:
                    import logging
                    logging.getLogger(__name__).warning(
                        "create_simulation: pre-purged %d orphan KMEs to avoid NOT NULL violation",
                        purged,
                    )

            if simulation_payload.status is None:
                simulation_payload = simulation_payload.model_copy(
                    update={"status": SimulationStatus.PENDING}
                )

            simulation_header = simulation_payload.model_copy(
                update={"id": None, "list_dkms": [], "sdn": simulation_payload.sdn}
            )
            saved_simulation = self.uow.repos.simulations.save(simulation_header)
            if saved_simulation.id is None:
                raise RuntimeError("No se pudo crear la simulacion")
            simulation_id = saved_simulation.id

            # En la UoW SQLAlchemy necesitamos acceso a la sesion para persistir SDN/KME,
            # ya que no existen repositorios dedicados para estos recursos.
            session = getattr(self.uow, "_session", None)
            queued_kmes: list[tuple[str, KMEConfig]] = []
            qkc_id_map: Dict[int, int] = {}
            qkc_cache: Dict[str, ModelQKC] = {}
            orr_cache: Dict[str, ModelORR] = {}
            persisted_dkms: list[ModelDKMS] = []

            for dkms_model in simulation_payload.list_dkms:
                dkms_model = self._bind_host_to_simulation(dkms_model, simulation_id)

                if dkms_model.orr is not None:
                    orr_model = self._bind_host_to_simulation(dkms_model.orr, simulation_id)

                    if orr_model.qkc is not None:
                        qkc_model = self._bind_host_to_simulation(orr_model.qkc, simulation_id)
                        qkc_key = self._entity_cache_key(qkc_model)
                        saved_qkc = qkc_cache.get(qkc_key)
                        if saved_qkc is None:
                            qkc_kmes = list(qkc_model.kmes)
                            qkc_to_save = (
                                qkc_model.model_copy(update={"kmes": []})
                                if session is not None
                                else qkc_model
                            )
                            saved_qkc = self.uow.repos.qkcs.save(qkc_to_save)
                            qkc_cache[qkc_key] = saved_qkc
                            if qkc_model.id is not None and saved_qkc.id is not None:
                                qkc_id_map[qkc_model.id] = saved_qkc.id
                            for kme_model in qkc_kmes:
                                queued_kmes.append((qkc_key, kme_model))
                        if saved_qkc.id is None:
                            raise RuntimeError("No se pudo persistir el QKC de la simulacion")
                        orr_model = orr_model.model_copy(
                            update={"qkc_id": saved_qkc.id, "qkc": saved_qkc}
                        )
                    elif self.uow.repos.qkcs.get(orr_model.qkc_id) is None:
                        raise ValueError(
                            f"El ORR {orr_model.id or '(sin id)'} referencia "
                            f"qkc_id={orr_model.qkc_id} inexistente"
                        )

                    orr_key = self._entity_cache_key(orr_model)
                    saved_orr = orr_cache.get(orr_key)
                    if saved_orr is None:
                        saved_orr = self.uow.repos.orrs.save(orr_model)
                        orr_cache[orr_key] = saved_orr
                    if saved_orr.id is None:
                        raise RuntimeError("No se pudo persistir el ORR de la simulacion")
                    dkms_model = dkms_model.model_copy(update={"orr_id": saved_orr.id, "orr": saved_orr})
                elif self.uow.repos.orrs.get(dkms_model.orr_id) is None:
                    raise ValueError(
                        f"El DKMS {dkms_model.id or '(sin id)'} referencia "
                        f"orr_id={dkms_model.orr_id} inexistente"
                    )

                persisted_dkms.append(self.uow.repos.dkms.save(dkms_model))

            persisted_sdn = self._bind_host_to_simulation(simulation_payload.sdn, simulation_id)
            if session is not None:
                from persistence.sqlalchemy.mappers import Entity2Model, Model2Entity

                sdn_entity = session.merge(Model2Entity.sdn(persisted_sdn))
                session.flush()
                persisted_sdn = Entity2Model.sdn(sdn_entity)

                for qkc_key, kme_model in queued_kmes:
                    owner_qkc = qkc_cache[qkc_key]
                    if owner_qkc.id is None:
                        raise RuntimeError("No se pudo resolver el QKC para persistir KME")
                    local_qkc_id = qkc_id_map.get(kme_model.local_qkc_id, kme_model.local_qkc_id)
                    neighbor_qkc_id = qkc_id_map.get(
                        kme_model.neighbor_qkc_id,
                        kme_model.neighbor_qkc_id,
                    )
                    if local_qkc_id is None:
                        local_qkc_id = owner_qkc.id
                    if neighbor_qkc_id is None:
                        raise ValueError("neighbor_qkc_id es obligatorio en la configuracion KME")
                    kme_to_save = kme_model.model_copy(
                        update={
                            "local_qkc_id": local_qkc_id,
                            "neighbor_qkc_id": neighbor_qkc_id,
                        }
                    )
                    session.merge(
                        Model2Entity.kme(kme_to_save, id_simulation=simulation_id)
                    )
                session.flush()

            completed_simulation = saved_simulation.model_copy(
                update={"list_dkms": persisted_dkms, "sdn": persisted_sdn}
            )
            self.uow.repos.simulations.save(completed_simulation)
            self.uow.commit()

            persisted = self.uow.repos.simulations.get(simulation_id)
            if persisted is None:
                raise RuntimeError(f"Simulacion {simulation_id} no encontrada tras persistencia")
            return persisted

    def get_simulations(self, user_id: int | str) -> list[ModelSimulation]:
        normalized_user_id = self._normalize_user_id(user_id)
        with self.uow:
            if self.uow.repos.users.get(normalized_user_id) is None:
                raise ValueError(f"Usuario {normalized_user_id} no encontrado")

            session = getattr(self.uow, "_session", None)
            if session is not None:
                from persistence.sqlalchemy.data import Simulation
                from persistence.sqlalchemy.mappers import Entity2Model

                entities = (
                    session.query(Simulation)
                    .filter(Simulation.id_user == normalized_user_id)
                    .order_by(Simulation.id.asc())
                    .all()
                )
                return [Entity2Model.simulation(entity) for entity in entities]

            return self._list_simulations_from_memory_repo(normalized_user_id)

    def get_simulation_id(
        self, user_id: int | str, id_simulation: int | str
    ) -> ModelSimulation:
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, _ = self._normalize_simulation_id(id_simulation)
        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)
            return sim

    def run_simulation(self, user_id: int | str, id_simulation: int | str) -> None:
        pods = self._load_pods_module()
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, simulation_key = self._normalize_simulation_id(id_simulation)
        # Purga preventiva de SAEs zombie del loadtest. Si un test anterior
        # falló al limpiar (SDN 503, client timeout, SIGKILL, …) sus SAEs
        # persisten en la DB y se re-registran en el SDN al relanzar la
        # sim, saturando el controlador con estado huérfano. Aquí borramos
        # cualquier SAE con prefijo ``lt-*`` antes de recrear la sim para
        # que el SDN arranque limpio. SAEs de usuarios reales (sin prefijo
        # loadtest) no se tocan.
        try:
            self._purge_loadtest_sae_residue(simulation_id)
        except Exception as exc:  # noqa: BLE001
            print(f"run_simulation: purge de SAEs loadtest falló (continuando): {exc}")
        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)
            if sim.status == SimulationStatus.RUNNING:
                raise ValueError(
                    f"La simulacion {simulation_id} ya esta en ejecucion. Detenla antes de volver a lanzarla."
                )
            if sim.sdn is None:
                raise ValueError("La simulacion no tiene SDN asociado")
            if not sim.list_dkms:
                raise ValueError(
                    "La simulacion no tiene DKMS configurados. "
                    "Anade al menos un nodo DKMS y guarda la topologia antes de ejecutar."
                )

            pod_sdn = pods.PodSDN(id_simulation=simulation_key, model_sdn=sim.sdn)
            self._wait_namespace_ready_for_run(pod_sdn, simulation_key)
            if getattr(pods, "K8S_RUNTIME_MTLS_ENABLED", False):
                try:
                    self._ensure_runtime_ca_secret(
                        simulation_id=simulation_id,
                        simulation_namespace=simulation_key,
                    )
                except Exception as exc:  # noqa: BLE001
                    raise ValueError(
                        "No se pudo preparar la Runtime CA de mTLS para la simulacion "
                        f"{simulation_id}: {exc}"
                    ) from exc
            sdn_host = getattr(sim.sdn, "host", None)
            if sdn_host is None:
                raise ValueError("La simulacion no tiene host SDN asociado")
            sdn_port = getattr(sdn_host, "port", None)
            if sdn_port is None:
                raise ValueError("La simulacion no tiene puerto SDN asociado")
            try:
                sdn_service_port = int(sdn_port)
            except (TypeError, ValueError) as exc:
                raise ValueError(f"Puerto SDN invalido: {sdn_port}") from exc
            # 2026-05-20: use FQDN (svc.<ns>.svc.cluster.local) instead of
            # the bare service name. Some EKS Auto Mode clusters started
            # rejecting short-name DNS lookups across namespaces, leaving
            # DKMS pods unable to POST /demand to the SDN. FQDN is always
            # resolvable regardless of search-domain config.
            sdn_service_host = f"{pod_sdn.name}.{simulation_key}.svc.cluster.local"

            dkms_pods: list[pods.PodDKMS] = []
            for dkms in sim.list_dkms:
                if dkms.id is None:
                    raise ValueError("El DKMS no tiene id asignado")
                dkms_pod = pods.PodDKMS(id_simulation=simulation_key, model_dkms=dkms)
                dkms_pods.append(dkms_pod)
                self.pods_dkms[(simulation_key, dkms.id)] = dkms_pod

            # v3.3: desplegar 1 pod quditto por enlace QKD (no por nodo).
            # Cada `PodQudittoLink` hostea AMBAS vistas (A y B) del enlace
            # en un único proceso simple_quditto; DKMS-A y DKMS-B hablan
            # con el mismo pod via DNS K8s. Deploy ANTES que los DKMS para
            # que las URLs ya resuelvan en el config del primer fetch
            # (aunque el prefetch worker tiene retry, así evitamos warnings
            # de transiente al arrancar).
            link_pods: list[pods.PodQudittoLink] = (
                pods.build_quditto_link_pods_from_dkms_models(
                    id_simulation=simulation_key,
                    dkms_models=sim.list_dkms,
                )
            )
            for link_pod in link_pods:
                self._deploy_pod(
                    link_pod,
                    image=self.image_quditto,
                    image_pull_secret=self.image_pull_secret,
                )

            if getattr(pods, "K8S_OBSERVABILITY_ENABLED", False):
                pod_observability = pods.PodObservability(id_simulation=simulation_key)
                pod_observability.deploy(
                    dkms_pods=dkms_pods,
                    sdn_pod=pod_sdn,
                    image_pull_secret=self.image_pull_secret,
                    image_pull_policy=self.image_pull_policy,
                    link_pods=link_pods,
                )
                self.pods_observability[simulation_key] = pod_observability

            sdn_env_vars = self._build_sdn_runtime_topology_env(
                sim,
                pods_module=pods,
            )
            # Rust SDN: generate per-entity topology JSONs into a
            # ConfigMap that the SDN Pod mounts at /app/topology.
            if getattr(pods, "K8S_DKMS_RUST_SIDECARS", False):
                try:
                    pod_sdn.ensure_rust_topology_configmap(sim.list_dkms or [])
                except Exception as exc:  # noqa: BLE001
                    raise ValueError(
                        f"No se pudo generar el ConfigMap de topología para SDN: {exc}"
                    ) from exc
            self._deploy_pod(
                pod_sdn,
                image=self.image_sdn,
                image_pull_secret=self.image_pull_secret,
                env_vars=sdn_env_vars or None,
            )
            if getattr(pods, "K8S_DKMS_RUST_SIDECARS", False):
                try:
                    pod_sdn.patch_with_rust_topology_volume()
                except Exception as exc:  # noqa: BLE001
                    raise ValueError(
                        f"No se pudo patchear el SDN con la topología: {exc}"
                    ) from exc
                # Expose the SDN Rust HTTP admin port (grpc+2) so
                # ``DKMS::generator`` can poll ``/rate/{dkms_id}``.
                try:
                    pod_sdn.expose_rust_http_port()
                except Exception as exc:  # noqa: BLE001
                    raise ValueError(
                        f"No se pudo exponer el puerto HTTP del SDN Rust: {exc}"
                    ) from exc
            self.pods_sdn[simulation_key] = pod_sdn

            # Pasar la topología completa a los DKMS para que cada nodo
            # conozca a todos sus vecinos (misma lógica que SDN_TOPOLOGY_JSON_B64).
            dkms_topology_b64 = sdn_env_vars.get("SDN_TOPOLOGY_JSON_B64", "")

            # Rust DKMS: build peer config map so the Generator can pick
            # peers with transport=orr and start filling buffer_enc.
            # Layout: dkms_id_by_qkc_id maps the qkc_id of a DKMS (which
            # is what shows up as `neighbor_qkc_id` in kmes) to the K8s
            # Service DNS of the owning DKMS pod and to its ORR id.
            dkms_meta_by_qkc_id: dict[int, dict[str, Any]] = {}
            for d in sim.list_dkms or []:
                orr_o = getattr(d, "orr", None)
                qkc_o = getattr(orr_o, "qkc", None) if orr_o else None
                dh = getattr(d, "host", None)
                if not (qkc_o and dh):
                    continue
                try:
                    qid = int(getattr(qkc_o, "id", 0))
                    hid = int(getattr(dh, "id", 0))
                    sae_port = int(getattr(dh, "port", 0) or 4001)
                except (TypeError, ValueError):
                    continue
                orr_id_int = int(getattr(orr_o, "id", 0) or qid)
                dkms_meta_by_qkc_id[qid] = {
                    "dns": f"dkms-{hid}",  # short name for config-rs map keys
                    # 2026-05-20: FQDN for URLs (avoids EKS short-name DNS issues)
                    "fqdn": f"dkms-{hid}.{simulation_key}.svc.cluster.local",
                    "sae_port": sae_port,
                    # peer_port = sae_port + 1 (mismo convenio que
                    # `_dkms_rust_env_overrides` en pods.py). El
                    # endpoint DKMS↔DKMS para `POST /kmapi/v1/ext_keys`
                    # va a esta puerta (router v020.rs, no v014.rs).
                    "peer_port": sae_port + 1,
                    "orr_id": f"orr-{orr_id_int}",
                }

            # Make the qkc.toml builder aware of the K8s DNS-name of each
            # neighbor DKMS pod (its Service is ``dkms-<host_id>``).
            # ``_qkc_runtime_toml`` reads this map to populate
            # ``neighbor_peer_addr`` properly instead of using the
            # legacy 127.0.0.x IP that lives in the model.
            k8s_dns_by_qkc_id = {qid: meta["dns"] for qid, meta in dkms_meta_by_qkc_id.items()}

            for dkms_pod in dkms_pods:
                # Plumb the qkc_id → DNS map into the pod object so its
                # `_qkc_runtime_toml` can resolve neighbor service hosts.
                setattr(dkms_pod, "_k8s_dns_by_qkc_id", k8s_dns_by_qkc_id)
                dkms_env = {
                    "DKMS_SDN_HOST": sdn_service_host,
                    "DKMS_SDN_PORT": str(sdn_service_port),
                }
                # Bug fix (2026-05-20): NOT injecting DKMS_TOPOLOGY_JSON_B64
                # — for N=40 RGG/SECOQC the JSON is ~186 KB and together
                # with the rest of the env (~16 KB) exceeds the kernel's
                # ARG_MAX (~128 KB on Linux), making the DKMS binary fail
                # at `execve` with "argument list too long". The Rust DKMS
                # never reads this var anyway (it queries the SDN at
                # runtime), so we just drop it. The legacy Python DKMS
                # used it but that path is deprecated.
                _ = dkms_topology_b64  # kept for backwards-compat / future use
                if getattr(pods, "K8S_DKMS_RUST_SIDECARS", False):
                    # Override Rust DKMS [buffer] + [generator] sections.
                    # The Environment loader replaces the whole section, so
                    # we must pass every field of each section that we
                    # touch. Defaults (config.rs) are 4096/1024/256 for
                    # buffer and 32 max_tokens/tick. Both are too low to
                    # let the generator hit the SDN-allocated rate on a
                    # busy Y topology — buffers saturate in < 15 s and the
                    # per-peer token cap pins emission at ~320 keys/s
                    # regardless of what the MCF solver allocates.
                    #
                    # Knobs configurables vía env (mantener compatibilidad
                    # con defaults previos 65536 / 16384 / 2048 / 400):
                    # cambiarlos via env evita tener que hacer `kubectl
                    # set env` post-launch sobre los DKMS, lo que disparaba
                    # un rollout y destruía el estado in-memory de los
                    # ORR/QKC sidecars (race de master_secret + key_id
                    # mismatch contra quditto-link). Ver post-mortem
                    # tests/results/n10-fulllogs-repro-v2-3runs/run-1.
                    _buf_cap = os.getenv("DKMS_BUFFER_CAPACITY_PER_PEER", "65536")
                    _buf_low = os.getenv("DKMS_BUFFER_REFILL_LOW_WATERMARK", "16384")
                    _buf_batch = os.getenv("DKMS_BUFFER_REFILL_BATCH", "2048")
                    _gen_tokens = os.getenv("DKMS_GENERATOR_MAX_TOKENS_PER_PEER_PER_TICK", "400")
                    dkms_env.update({
                        "DKMS__buffer__capacity_per_peer":   _buf_cap,
                        "DKMS__buffer__refill_low_watermark": _buf_low,
                        "DKMS__buffer__refill_batch":         _buf_batch,
                        "DKMS__generator__enabled":           "true",
                        "DKMS__generator__key_size_bytes":    "32",
                        "DKMS__generator__tick_ms":           "100",
                        "DKMS__generator__rate_refresh_ms":   "1000",
                        "DKMS__generator__priority_refresh_ms":"200",
                        "DKMS__generator__ack_timeout_ms":    "30000",
                        "DKMS__generator__ack_reaper_ms":     "1000",
                        "DKMS__generator__max_tokens_per_peer_per_tick": _gen_tokens,
                        "DKMS__generator__bucket_cap_seconds":"2.0",
                    })
                    # ORR↔ORR sessions are PQC (ML-KEM-768) over gRPC,
                    # not QKD. Any pair of ORRs reachable on the K8s
                    # network can bootstrap a shared master_secret,
                    # regardless of whether their QKCs are directly
                    # linked by Quditto. Enumerate **every** other DKMS
                    # in the simulation as a peer so:
                    #   - the DKMS generator fills buffer_enc[peer] for
                    #     all destinations, not only QKD-direct ones;
                    #   - the ORR bootstraps a master_secret with every
                    #     other ORR up-front (lexicographic-initiator
                    #     rule in `orr/src/bootstrap.rs` guarantees one
                    #     encap per pair);
                    #   - the SDN priority registry receives a class
                    #     report for every flow and never has to fall
                    #     back to the default Priority for unobserved
                    #     pairs.
                    my_model = dkms_pod.model
                    my_orr = getattr(my_model, "orr", None)
                    my_qkc = getattr(my_orr, "qkc", None) if my_orr else None
                    my_qkc_id = int(getattr(my_qkc, "id", 0) or 0)
                    for n_qkc_id, meta in dkms_meta_by_qkc_id.items():
                        if n_qkc_id == my_qkc_id:
                            continue
                        peer_key = meta["dns"]  # e.g. "dkms-15" (no dots — config-rs uses '.' as separator)
                        # peer-plane port (sae_port+1) — `POST /kmapi/v1/ext_keys`
                        # vive en el router ETSI 020 del peer_addr, no en el
                        # sae_addr (que es para SAE-facing ETSI 014).
                        # FQDN used in URL values (not in map keys) to avoid
                        # short-name DNS resolution issues under EKS Auto Mode.
                        dkms_env[f"DKMS__peers__{peer_key}__endpoint"] = (
                            f"https://{meta['fqdn']}:{meta['peer_port']}"
                        )
                        dkms_env[f"DKMS__peers__{peer_key}__transport"] = "orr"
                        dkms_env[f"DKMS__peers__{peer_key}__orr_id"] = meta["orr_id"]
                        dkms_env[f"ORR__peers__{meta['orr_id']}"] = str(n_qkc_id)
                        dkms_env[f"ORR__peer_grpc_addrs__{meta['orr_id']}"] = (
                            f"http://{meta['fqdn']}:50052"
                        )
                self._deploy_pod(
                    dkms_pod,
                    image=self.image_dkms,
                    image_pull_secret=self.image_pull_secret,
                    env_vars=dkms_env,
                    quditto_image=self.image_quditto,
                )

            pod_sdn.create_simulation_ingress(dkms_pods=dkms_pods, sdn_pod=pod_sdn)
            try:
                self._reconcile_sae_bindings_to_sdn(
                    simulation_id=simulation_id,
                    simulation_key=simulation_key,
                    simulation=sim,
                )
            except Exception as exc:  # noqa: BLE001
                raise ValueError(
                    "Fallo de reconciliacion SAE (DB -> SDN) durante run_simulation: "
                    f"{exc}"
                ) from exc

            sim.status = SimulationStatus.RUNNING
            self.uow.repos.simulations.save(sim)
            self.uow.commit()

    def stop_simulation(self, user_id: int | str, id_simulation: int | str) -> None:
        pods = self._load_pods_module()
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, simulation_key = self._normalize_simulation_id(id_simulation)
        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)

            pod_sdn = self.pods_sdn.get(simulation_key)
            if pod_sdn is None:
                if sim.sdn is not None:
                    try:
                        pod_sdn = pods.PodSDN(id_simulation=simulation_key, model_sdn=sim.sdn)
                    except ValueError as exc:
                        if not self._is_missing_model_id_error(exc):
                            raise
                if pod_sdn is None:
                    pod_sdn = self._build_namespace_only_pod(simulation_key, pods)

            pod_sdn.eliminar_namespace()

            if not self._wait_namespace_deleted(pod_sdn):
                raise ValueError(
                    "La infraestructura sigue deteniendose. Espera a que termine el borrado del namespace e intentalo de nuevo."
                )

            self._forget_simulation_pods(simulation_key)

            sim.status = SimulationStatus.FINISHED
            self.uow.repos.simulations.save(sim)
            self.uow.commit()

    def stop_dkms(
        self,
        user_id: int | str,
        id_simulation: int | str,
        id_dkms: int | str,
    ) -> None:
        pods = self._load_pods_module()
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, simulation_key = self._normalize_simulation_id(id_simulation)
        dkms_id = self._normalize_dkms_id(id_dkms)

        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)
            if sim.status != SimulationStatus.RUNNING:
                raise ValueError(
                    f"La simulacion {simulation_id} no esta en ejecucion. "
                    "Solo se pueden parar DKMS en simulaciones RUNNING."
                )

            dkms_id = self._resolve_dkms_id(sim, dkms_id)
            model_dkms = next(
                (dkms for dkms in sim.list_dkms if int(getattr(dkms, "id", 0) or 0) == dkms_id),
                None,
            )
            if model_dkms is None:
                raise ValueError(f"No existe el DKMS {dkms_id} en la simulacion {simulation_id}")

            pod_dkms = self.pods_dkms.get((simulation_key, dkms_id))
            if pod_dkms is None:
                pod_dkms = pods.PodDKMS(id_simulation=simulation_key, model_dkms=model_dkms)

            pod_dkms.eliminar_pod()

            core_api = getattr(pod_dkms, "core_v1_api", None)
            pod_name = getattr(pod_dkms, "name", None)
            namespace = getattr(pod_dkms, "namespace", None)
            if core_api is not None and pod_name and namespace:
                try:
                    core_api.delete_namespaced_service(name=pod_name, namespace=namespace)
                except Exception as exc:  # noqa: BLE001
                    if getattr(exc, "status", None) != 404:
                        raise

            self.pods_dkms.pop((simulation_key, dkms_id), None)

    def start_dkms(
        self,
        user_id: int | str,
        id_simulation: int | str,
        id_dkms: int | str,
    ) -> None:
        pods = self._load_pods_module()
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, simulation_key = self._normalize_simulation_id(id_simulation)
        dkms_id = self._normalize_dkms_id(id_dkms)

        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)
            if sim.status != SimulationStatus.RUNNING:
                raise ValueError(
                    f"La simulacion {simulation_id} no esta en ejecucion. "
                    "Solo se pueden arrancar DKMS en simulaciones RUNNING."
                )

            dkms_id = self._resolve_dkms_id(sim, dkms_id)
            model_dkms = next(
                (dkms for dkms in sim.list_dkms if int(getattr(dkms, "id", 0) or 0) == dkms_id),
                None,
            )
            if model_dkms is None:
                raise ValueError(f"No existe el DKMS {dkms_id} en la simulacion {simulation_id}")

            pod_sdn = self.pods_sdn.get(simulation_key)
            if pod_sdn is None:
                if sim.sdn is None:
                    raise ValueError(
                        f"La simulacion {simulation_id} no tiene SDN asociado para arrancar DKMS."
                    )
                pod_sdn = pods.PodSDN(id_simulation=simulation_key, model_sdn=sim.sdn)

            # 2026-05-20: FQDN (see comment at line ~887) — short-name
            # DNS lookups break under recent EKS Auto Mode CoreDNS configs.
            sdn_service_host = f"{pod_sdn.name}.{simulation_key}.svc.cluster.local"
            sdn_service_port = getattr(getattr(pod_sdn.model, "host", None), "port", None)
            if not sdn_service_port:
                raise ValueError("No se pudo resolver el puerto del servicio SDN para arrancar DKMS.")

            pod_dkms = self.pods_dkms.get((simulation_key, dkms_id))
            if pod_dkms is None:
                pod_dkms = pods.PodDKMS(id_simulation=simulation_key, model_dkms=model_dkms)

            self._deploy_pod(
                pod_dkms,
                image=self.image_dkms,
                image_pull_secret=self.image_pull_secret,
                env_vars={
                    "DKMS_SDN_HOST": sdn_service_host,
                    "DKMS_SDN_PORT": str(sdn_service_port),
                },
                quditto_image=self.image_quditto,
            )
            self.pods_dkms[(simulation_key, dkms_id)] = pod_dkms

    def delete_simulation(self, user_id: int | str, id_simulation: int | str) -> None:
        normalized_user_id = self._normalize_user_id(user_id)
        simulation_id, _ = self._normalize_simulation_id(id_simulation)

        self.stop_simulation(normalized_user_id, simulation_id)

        with self.uow:
            sim = self._get_simulation(simulation_id)
            self._ensure_simulation_owner(sim, normalized_user_id)
            deleted = self.uow.repos.simulations.delete(simulation_id)
            if not deleted:
                raise ValueError(f"Simulacion {simulation_id} no encontrada")
            self.uow.commit()


if __name__ == "__main__":
    url = os.getenv('DB_URL')

    from persistence import build_uow_from_env

    uow = build_uow_from_env()
    orchestator = Orchestator(url=url,uow=uow)

    import seed_db_from_configs
    seed_db_from_configs.main()

    orchestator.run_simulation(user_id=1,id_simulation=1)
    orchestator.stop_simulation(user_id=1,id_simulation=1)
    orchestator.delete_simulation(user_id=1,id_simulation=1)
