#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path
from typing import Iterable, Optional

from sqlalchemy import create_engine, inspect, text
from sqlalchemy.orm import Session, sessionmaker

# Ensure the project src/ is on sys.path when running as a script
ROOT = Path(__file__).resolve().parents[1]
if str(ROOT) not in sys.path:
    sys.path.append(str(ROOT))

from models import (  # noqa: E402
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
    TLSConfigSAE,
)


from models.enums import HTTPType  # noqa: E402
from persistence.sqlalchemy.data import Base  # noqa: E402
from persistence.sqlalchemy.mappers import Model2Entity  # noqa: E402


def _utc_now() -> datetime:
    return datetime.now(timezone.utc)


def _clear_database(session: Session) -> None:
    bind = session.get_bind()
    if bind is None:
        return

    inspector = inspect(bind)
    existing_tables = set(inspector.get_table_names())
    sorted_tables = [table for table in Base.metadata.sorted_tables if table.name in existing_tables]
    tables = [f'"{table.name}"' for table in sorted_tables]
    if not tables:
        return
    try:
        session.execute(text(f"TRUNCATE {', '.join(tables)} RESTART IDENTITY CASCADE"))
        session.commit()
    except Exception:
        session.rollback()
        for table in reversed(sorted_tables):
            session.execute(table.delete())
        session.commit()


def _ensure_kme_schema_compat(session: Session) -> None:
    """
    Compatibilidad con despliegues antiguos:
    si faltan columnas nuevas de KME, se crean antes del seed.
    """
    bind = session.get_bind()
    if bind is None:
        return

    inspector = inspect(bind)
    table_names = set(inspector.get_table_names())
    if "kme" not in table_names:
        return

    existing_columns = {col["name"] for col in inspector.get_columns("kme")}
    required_columns = {
        "channel_type": "channel_type NOT NULL DEFAULT 'qkd'",
        "channel_distance": "integer NOT NULL DEFAULT 0",
        "channel_quditto_max_buffer_size": "integer NOT NULL DEFAULT 10000",
        "channel_quditto_rate_r0": "double precision NOT NULL DEFAULT 2000.0",
        "channel_quditto_rate_alpha": "double precision NOT NULL DEFAULT 0.2",
        "pqc_simulation": "boolean NOT NULL DEFAULT false",
        "hybrid_enabled": "boolean NOT NULL DEFAULT false",
        "pqc_kme_port": "integer NOT NULL DEFAULT 6000",
    }
    missing_columns = [name for name in required_columns if name not in existing_columns]
    if not missing_columns:
        return

    if bind.dialect.name != "postgresql":
        raise RuntimeError(
            "La tabla kme no tiene el esquema esperado y la migracion automatica "
            "solo esta soportada para PostgreSQL."
        )

    try:
        if "channel_type" in missing_columns:
            session.execute(
                text(
                    """
                    DO $$
                    BEGIN
                        CREATE TYPE channel_type AS ENUM ('pqc-simulation', 'qkd');
                    EXCEPTION
                        WHEN duplicate_object THEN NULL;
                    END $$;
                    """
                )
            )

        for column_name in missing_columns:
            session.execute(
                text(
                    f'ALTER TABLE "kme" '
                    f'ADD COLUMN IF NOT EXISTS "{column_name}" {required_columns[column_name]};'
                )
            )
        session.commit()
        print(
            f"[info] kme: columnas creadas por compatibilidad: {', '.join(missing_columns)}"
        )
    except Exception:
        session.rollback()
        raise


def _ensure_web_schema_compat(session: Session) -> None:
    """
    Compatibilidad con despliegues antiguos para el modelo web:
    - simulation.editor_topology_json / created_at / updated_at
    - sdn.type_http
    - tabla simulation_run (+ enum simulation_run_status)
    """
    bind = session.get_bind()
    if bind is None:
        return

    if bind.dialect.name != "postgresql":
        return

    inspector = inspect(bind)
    table_names = set(inspector.get_table_names())

    changes: list[str] = []

    if "simulation" in table_names:
        simulation_columns = {col["name"] for col in inspector.get_columns("simulation")}
        if "editor_topology_json" not in simulation_columns:
            session.execute(
                text(
                    'ALTER TABLE "simulation" '
                    'ADD COLUMN IF NOT EXISTS "editor_topology_json" text;'
                )
            )
            changes.append("simulation.editor_topology_json")
        if "created_at" not in simulation_columns:
            session.execute(
                text(
                    'ALTER TABLE "simulation" '
                    'ADD COLUMN IF NOT EXISTS "created_at" timestamptz NOT NULL DEFAULT now();'
                )
            )
            changes.append("simulation.created_at")
        if "updated_at" not in simulation_columns:
            session.execute(
                text(
                    'ALTER TABLE "simulation" '
                    'ADD COLUMN IF NOT EXISTS "updated_at" timestamptz NOT NULL DEFAULT now();'
                )
            )
            changes.append("simulation.updated_at")

    if "sdn" in table_names:
        sdn_columns = {col["name"] for col in inspector.get_columns("sdn")}
        if "type_http" not in sdn_columns:
            session.execute(
                text(
                    """
                    DO $$
                    BEGIN
                        CREATE TYPE http_type AS ENUM ('http', 'https');
                    EXCEPTION
                        WHEN duplicate_object THEN NULL;
                    END $$;
                    """
                )
            )
            session.execute(
                text(
                    'ALTER TABLE "sdn" '
                    'ADD COLUMN IF NOT EXISTS "type_http" http_type NOT NULL DEFAULT \'http\';'
                )
            )
            changes.append("sdn.type_http")

    session.execute(
        text(
            """
            DO $$
            BEGIN
                CREATE TYPE simulation_run_status AS ENUM ('QUEUED', 'RUNNING', 'DONE', 'FAILED');
            EXCEPTION
                WHEN duplicate_object THEN NULL;
            END $$;
            """
        )
    )
    session.execute(
        text(
            """
            CREATE TABLE IF NOT EXISTS simulation_run (
                id SERIAL PRIMARY KEY,
                simulation_id INTEGER NOT NULL REFERENCES simulation(id) ON DELETE CASCADE,
                status simulation_run_status NOT NULL,
                message TEXT NULL,
                queued_at TIMESTAMPTZ NULL,
                started_at TIMESTAMPTZ NULL,
                finished_at TIMESTAMPTZ NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT now()
            );
            """
        )
    )
    session.execute(
        text(
            """
            CREATE INDEX IF NOT EXISTS ix_simulation_run_simulation_id
            ON simulation_run(simulation_id);
            """
        )
    )
    session.execute(
        text(
            """
            CREATE INDEX IF NOT EXISTS ix_simulation_run_status
            ON simulation_run(status);
            """
        )
    )
    changes.append("simulation_run")

    session.commit()
    if changes:
        unique_changes = sorted(set(changes))
        print(f"[info] web-schema: compat aplicada: {', '.join(unique_changes)}")


def _ensure_sae_schema_compat(session: Session) -> None:
    """
    Compatibilidad para el modelo SAE con lifecycle/certificados.
    """
    bind = session.get_bind()
    if bind is None or bind.dialect.name != "postgresql":
        return

    inspector = inspect(bind)
    table_names = set(inspector.get_table_names())
    if "sae" not in table_names:
        return

    existing_columns = {col["name"] for col in inspector.get_columns("sae")}
    changes: list[str] = []

    session.execute(
        text(
            """
            DO $$
            BEGIN
                CREATE TYPE sae_status AS ENUM ('pending_cert', 'active', 'revoked', 'expired');
            EXCEPTION
                WHEN duplicate_object THEN NULL;
            END $$;
            """
        )
    )

    required_columns = {
        "sae_id": "text",
        "display_name": "text",
        "owner_user_id": "integer",
        "simulation_id": "integer",
        "dkms_id": "integer",
        "status": "sae_status NOT NULL DEFAULT 'pending_cert'",
        "cert_serial": "text",
        "cert_fingerprint": "text",
        "cert_subject": "text",
        "cert_not_before": "timestamptz",
        "cert_not_after": "timestamptz",
        "revoked_at": "timestamptz",
        "revocation_reason": "text",
        "created_at": "timestamptz NOT NULL DEFAULT now()",
        "updated_at": "timestamptz NOT NULL DEFAULT now()",
    }
    for column_name, sql_type in required_columns.items():
        if column_name in existing_columns:
            continue
        session.execute(
            text(
                f'ALTER TABLE "sae" '
                f'ADD COLUMN IF NOT EXISTS "{column_name}" {sql_type};'
            )
        )
        changes.append(f"sae.{column_name}")

    for nullable_column in ("sdn_id", "tls_id", "agent_dkms_id"):
        if nullable_column in existing_columns:
            session.execute(
                text(
                    f'ALTER TABLE "sae" ALTER COLUMN "{nullable_column}" DROP NOT NULL;'
                )
            )

    session.execute(
        text(
            """
            DO $$
            BEGIN
                ALTER TABLE "sae"
                ADD CONSTRAINT fk_sae_owner_user
                FOREIGN KEY (owner_user_id) REFERENCES "user"(id)
                ON UPDATE CASCADE ON DELETE SET NULL;
            EXCEPTION
                WHEN duplicate_object THEN NULL;
            END $$;
            """
        )
    )
    session.execute(
        text(
            """
            DO $$
            BEGIN
                ALTER TABLE "sae"
                ADD CONSTRAINT fk_sae_simulation
                FOREIGN KEY (simulation_id) REFERENCES simulation(id)
                ON UPDATE CASCADE ON DELETE SET NULL;
            EXCEPTION
                WHEN duplicate_object THEN NULL;
            END $$;
            """
        )
    )
    session.execute(
        text(
            """
            DO $$
            BEGIN
                ALTER TABLE "sae"
                ADD CONSTRAINT fk_sae_dkms
                FOREIGN KEY (dkms_id) REFERENCES dkms(id)
                ON UPDATE CASCADE ON DELETE SET NULL;
            EXCEPTION
                WHEN duplicate_object THEN NULL;
            END $$;
            """
        )
    )
    session.execute(text('DROP INDEX IF EXISTS uq_sae_sae_id;'))
    session.execute(
        text(
            'CREATE UNIQUE INDEX IF NOT EXISTS uq_sae_simulation_sae_id '
            'ON "sae"(simulation_id, sae_id) '
            'WHERE simulation_id IS NOT NULL AND sae_id IS NOT NULL;'
        )
    )
    session.execute(
        text(
            'CREATE INDEX IF NOT EXISTS ix_sae_simulation_id ON "sae"(simulation_id);'
        )
    )
    session.execute(
        text(
            'CREATE INDEX IF NOT EXISTS ix_sae_dkms_id ON "sae"(dkms_id);'
        )
    )
    session.execute(
        text(
            'CREATE INDEX IF NOT EXISTS ix_sae_fingerprint_status ON "sae"(cert_fingerprint, status);'
        )
    )

    session.commit()
    if changes:
        print(f"[info] sae-schema: compat aplicada: {', '.join(changes)}")


def _sync_postgres_id_sequences(session: Session) -> None:
    """
    Ajusta secuencias SERIAL/BIGSERIAL al max(id) actual para evitar
    duplicate key en inserts posteriores cuando el seed fija IDs explícitos.
    """
    bind = session.get_bind()
    if bind is None or bind.dialect.name != "postgresql":
        return

    for table in Base.metadata.sorted_tables:
        id_col = table.columns.get("id")
        if id_col is None:
            continue
        table_name = table.name
        session.execute(
            text(
                f"""
                WITH max_id AS (
                    SELECT MAX(id)::bigint AS value FROM "{table_name}"
                )
                SELECT setval(
                    pg_get_serial_sequence('"{table_name}"', 'id'),
                    GREATEST(COALESCE((SELECT value FROM max_id), 1), 1),
                    COALESCE((SELECT value FROM max_id), 0) > 0
                );
                """
            )
        )


def _load_json_files(folder: Path) -> Iterable[tuple[str, dict]]:
    if not folder.exists():
        return []
    files = sorted(folder.glob("*.json"))
    for file_path in files:
        with file_path.open("r", encoding="utf-8") as handle:
            yield file_path.name, json.load(handle)


def _normalize_payload(kind: str, payload: dict) -> dict:
    normalized = dict(payload)
    if kind == "QKC" and "id_host" not in normalized and "host_id" in normalized:
        normalized["id_host"] = normalized.get("host_id")
    if kind == "ORR" and "qkc_id" not in normalized and "QKC_id" in normalized:
        normalized["qkc_id"] = normalized.get("QKC_id")
    if kind == "DKMS" and "orr_id" not in normalized and "ORR_id" in normalized:
        normalized["orr_id"] = normalized.get("ORR_id")
    return normalized


def _attach_host(model, simulation_id: int) -> tuple[object, Optional[ModelHost]]:
    host = getattr(model, "host", None)
    if host is not None:
        host_id = host.id or getattr(model, "id_host", None)
        if host_id is None:
            raise ValueError(f"{type(model).__name__} requiere host.id o id_host para asociar Host")
        host = host.model_copy(update={"id_simulation": simulation_id, "id": host_id})
        model = model.model_copy(update={"id_host": host_id, "host": host})
        return model, host

    ip = getattr(model, "ip", None)
    port = getattr(model, "port", None)
    host_id = getattr(model, "id_host", None)
    if ip is None or port is None:
        return model, None
    host = ModelHost(
        id=host_id,
        id_simulation=simulation_id,
        ip=str(ip),
        port=int(port),
    )
    model = model.model_copy(update={"id_host": host.id, "host": host})
    return model, host


def _persist_hosts(session: Session, hosts: Iterable[ModelHost]) -> None:
    for host in hosts:
        session.merge(Model2Entity.host(host))
    session.flush()


def _persist_group(session: Session, models: Iterable, mapper) -> None:
    for model in models:
        session.merge(mapper(model))
    session.flush()


def _load_models(config_dir: Path, kind: str, model_cls):
    models = []
    for name, payload in _load_json_files(config_dir / kind):
        normalized = _normalize_payload(kind, payload)
        try:
            model = model_cls.model_validate(normalized)
        except Exception as exc:
            print(f"[warn] {kind} {name}: {exc}")
            continue
        models.append(model)
    return models


def _parse_sae_id(raw: object) -> Optional[int]:
    if isinstance(raw, int):
        return raw
    if isinstance(raw, str):
        digits = "".join(ch for ch in raw if ch.isdigit())
        if digits:
            return int(digits)
    return None


def _build_endpoint_map(models: Iterable) -> dict[tuple[str, int], int]:
    mapping: dict[tuple[str, int], int] = {}
    for model in models:
        host = getattr(model, "host", None)
        if host is None:
            continue
        try:
            key = (str(host.ip), int(host.port))
        except (TypeError, ValueError):
            continue
        model_id = getattr(model, "id", None)
        if model_id is None:
            continue
        mapping[key] = int(model_id)
    return mapping


def _build_endpoint_map_from_db(session: Session, table_name: str) -> dict[tuple[str, int], int]:
    if table_name not in {"dkms", "sdn"}:
        raise ValueError(f"Tabla no soportada para endpoint map: {table_name}")

    rows = session.execute(
        text(
            f"""
            SELECT t.id AS id, h.ip AS ip, h.port AS port
            FROM "{table_name}" t
            JOIN "host" h ON h.id = t.id_host
            """
        )
    )
    mapping: dict[tuple[str, int], int] = {}
    for row in rows:
        try:
            key = (str(row.ip), int(row.port))
            mapping[key] = int(row.id)
        except (TypeError, ValueError):
            continue
    return mapping


def _pick_default_id(mapping: dict[tuple[str, int], int]) -> Optional[int]:
    if not mapping:
        return None
    return min(set(mapping.values()))


def _load_sae_payloads(config_dir: Path) -> list[tuple[str, dict]]:
    return list(_load_json_files(config_dir / "SAE"))


def _persist_agent_controllers(
    session: Session,
    models: Iterable[ModelAgentController],
    dkms_map: dict[tuple[str, int], int],
    valid_sdn_ids: set[int],
    default_sdn_id: Optional[int],
) -> dict[int, int]:
    valid_dkms_ids = set(dkms_map.values())
    controllers_by_dkms: dict[int, int] = {}
    skipped = 0
    dkms_ids_by_ip: dict[str, set[int]] = {}
    for (ip, _port), dkms_id in dkms_map.items():
        dkms_ids_by_ip.setdefault(str(ip), set()).add(int(dkms_id))

    for model in models:
        resolved_dkms_id = model.id_dkms if model.id_dkms in valid_dkms_ids else None
        if resolved_dkms_id is None:
            host = getattr(model, "host", None)
            if host is not None:
                try:
                    resolved_dkms_id = dkms_map.get((str(host.ip), int(host.port)))
                except (TypeError, ValueError):
                    resolved_dkms_id = None
                if resolved_dkms_id is None:
                    ip_candidates = sorted(dkms_ids_by_ip.get(str(getattr(host, "ip", "")), set()))
                    if len(ip_candidates) == 1:
                        resolved_dkms_id = int(ip_candidates[0])

        if resolved_dkms_id is None:
            skipped += 1
            print(
                f"[warn] AgentController id={model.id}: "
                f"no se pudo resolver id_dkms={model.id_dkms}; omitido."
            )
            continue

        resolved_sdn_id = model.id_sdn
        if resolved_sdn_id is not None and resolved_sdn_id not in valid_sdn_ids:
            resolved_sdn_id = default_sdn_id
        if resolved_sdn_id is not None and resolved_sdn_id not in valid_sdn_ids:
            resolved_sdn_id = None

        resolved_model = model.model_copy(
            update={"id_dkms": resolved_dkms_id, "id_sdn": resolved_sdn_id}
        )
        entity = session.merge(Model2Entity.agent_controller(resolved_model))
        session.flush()
        if entity.id_dkms is None or entity.id is None:
            continue
        controllers_by_dkms[int(entity.id_dkms)] = int(entity.id)

    if skipped:
        print(f"[warn] AgentControllers omitidos por FK no resoluble: {skipped}")
    return controllers_by_dkms


def _build_sae_input(
    payload: dict,
    sdn_map: dict[tuple[str, int], int],
    default_sdn_id: Optional[int],
    dkms_map: dict[tuple[str, int], int],
    controllers_by_dkms: dict[int, int],
) -> Optional[dict]:
    dkms_target = payload.get("dkms_target") or {}
    dkms_ip = dkms_target.get("ip")
    dkms_port = dkms_target.get("port")
    if dkms_ip is None or dkms_port is None:
        return None
    dkms_id = dkms_map.get((str(dkms_ip), int(dkms_port)))
    if dkms_id is None:
        return None
    controller_id = controllers_by_dkms.get(dkms_id)
    if controller_id is None:
        return None

    sdn_payload = payload.get("sdn") or {}
    sdn_ip = sdn_payload.get("ip")
    sdn_port = sdn_payload.get("port")
    sdn_id = None
    if sdn_ip is not None and sdn_port is not None:
        sdn_id = sdn_map.get((str(sdn_ip), int(sdn_port)))
    if sdn_id is None:
        sdn_id = default_sdn_id
    if sdn_id is None:
        return None

    tls_payload = payload.get("tls") or {}
    cert_path = (tls_payload.get("cert") or {}).get("path")
    key_path = (tls_payload.get("key") or {}).get("path")
    ca_path = (tls_payload.get("ca_certs") or {}).get("path")
    tls_model: Optional[TLSConfigSAE] = None
    status = SaeStatus.PENDING_CERT
    if cert_path and key_path and ca_path:
        cert_exists = Path(str(cert_path)).is_file()
        key_exists = Path(str(key_path)).is_file()
        ca_exists = Path(str(ca_path)).is_file()
        if cert_exists and key_exists and ca_exists:
            tls_model = TLSConfigSAE(
                cert=ModelFile(path=str(cert_path)),
                key=ModelFile(path=str(key_path)),
                ca_certs=ModelFile(path=str(ca_path)),
                use_client_cert=bool(tls_payload.get("use_client_cert", False)),
            )
            status = SaeStatus.ACTIVE

    return {
        "id": _parse_sae_id(payload.get("id")),
        "sae_id": str(payload.get("id") or "").strip() or None,
        "sdn_id": sdn_id,
        "agent_dkms_id": controller_id,
        "dkms_id": dkms_id,
        "tls": tls_model,
        "status": status,
    }


def _persist_tls_sae(session: Session, tls_model: TLSConfigSAE) -> int:
    entity = session.merge(Model2Entity.tls_sae(tls_model))
    session.flush()
    if entity.id is None:
        raise RuntimeError("No se pudo persistir tls_config_sae")
    return int(entity.id)


def parse_args() -> argparse.Namespace:

    from conf import CONFIG_FOLDER


    parser = argparse.ArgumentParser(description="Reset DB and seed from config_files")
    parser.add_argument(
        "--db-url",
        default=os.environ.get("DB_URL"),
        help="SQLAlchemy database URL (or set DB_URL env var)",
    )
    parser.add_argument(
        "--config-dir",
        type=Path,
        default=CONFIG_FOLDER,
        help="Path to config_files directory",
    )
    parser.add_argument("--user-name", default="config_user", help="Username for seed user")
    parser.add_argument("--user-email", default="config_user@example.com", help="Email for seed user")
    parser.add_argument("--user-pass", default="config_password", help="Password hash for seed user")
    parser.add_argument("--simulation-name", default="config_simulation", help="Simulation name")
    parser.add_argument("--simulation-desc", default="seeded from config_files", help="Simulation description")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if not args.db_url:
        print("[error] DB_URL is required (or pass --db-url).")
        return 1

    engine = create_engine(args.db_url)
    SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=engine)

    with SessionLocal() as session:
        _clear_database(session)
        _ensure_kme_schema_compat(session)
        _ensure_web_schema_compat(session)
        _ensure_sae_schema_compat(session)

        user = ModelUser(
            username=args.user_name,
            email=args.user_email,
            password_hash=args.user_pass,
            is_active=True,
        )
        user_entity = session.merge(Model2Entity.user(user))
        session.flush()
        user_id = user_entity.id

        placeholder_sdn = ModelSDN(id=None, id_host=None, host=None, type_http=HTTPType.HTTPS)
        simulation = ModelSimulation(
            id_user=user_id,
            name=args.simulation_name,
            description=args.simulation_desc,
            start_time=_utc_now(),
            end_time=None,
            status=SimulationStatus.PENDING.value,
            user=user.model_copy(update={"id": user_id}),
            list_dkms=[],
            sdn=placeholder_sdn,
        )
        sim_entity = session.merge(Model2Entity.simulation(simulation))
        session.flush()
        simulation_id = sim_entity.id

        qkc_models = _load_models(args.config_dir, "QKC", ModelQKC)
        orr_models = _load_models(args.config_dir, "ORR", ModelORR)
        dkms_models = _load_models(args.config_dir, "DKMS", ModelDKMS)
        sdn_models = _load_models(args.config_dir, "SDN", ModelSDN)
        agent_models = _load_models(args.config_dir, "AgentControllers", ModelAgentController)
        sae_payloads = _load_sae_payloads(args.config_dir)

        kme_models = []
        normalized_qkcs = []
        hosts_by_id: dict[int, ModelHost] = {}

        # Insert QKC without KMEs first to avoid FK issues with neighbor_qkc_id.
        for qkc in qkc_models:
            kme_models.extend(list(getattr(qkc, "kmes", [])))
            qkc = qkc.model_copy(update={"kmes": []})
            qkc, host = _attach_host(qkc, simulation_id)
            if host and host.id is not None:
                hosts_by_id[host.id] = host
            normalized_qkcs.append(qkc)

        normalized_orrs = []
        for orr in orr_models:
            orr, host = _attach_host(orr, simulation_id)
            if host and host.id is not None:
                hosts_by_id[host.id] = host
            normalized_orrs.append(orr)

        normalized_dkms = []
        for dkms in dkms_models:
            dkms = dkms.model_copy(update={"orr": None})
            dkms, host = _attach_host(dkms, simulation_id)
            if host and host.id is not None:
                hosts_by_id[host.id] = host
            normalized_dkms.append(dkms)

        normalized_sdns = []
        for sdn in sdn_models:
            sdn, host = _attach_host(sdn, simulation_id)
            if host and host.id is not None:
                hosts_by_id[host.id] = host
            normalized_sdns.append(sdn)

        normalized_agents = []
        for agent in agent_models:
            agent, host = _attach_host(agent, simulation_id)
            if host and host.id is not None:
                hosts_by_id[host.id] = host
            normalized_agents.append(agent)

        if not normalized_dkms:
            raise RuntimeError(
                f"No se encontraron configuraciones DKMS en {args.config_dir / 'DKMS'}"
            )
        if not normalized_sdns:
            raise RuntimeError(
                f"No se encontraron configuraciones SDN en {args.config_dir / 'SDN'}"
            )

        _persist_hosts(session, hosts_by_id.values())
        _persist_group(session, normalized_qkcs, Model2Entity.qkc)
        _persist_group(session, normalized_orrs, Model2Entity.orr)
        _persist_group(session, normalized_dkms, Model2Entity.dkms)
        _persist_group(session, normalized_sdns, Model2Entity.sdn)

        dkms_map = _build_endpoint_map_from_db(session, "dkms")
        sdn_map = _build_endpoint_map_from_db(session, "sdn")
        default_sdn_id = _pick_default_id(sdn_map)
        valid_sdn_ids = set(sdn_map.values())

        controllers_by_dkms = _persist_agent_controllers(
            session,
            normalized_agents,
            dkms_map=dkms_map,
            valid_sdn_ids=valid_sdn_ids,
            default_sdn_id=default_sdn_id,
        )

        for kme in kme_models:
            # KME payloads may reference DataFile IDs already attached to other
            # entities in the same seed run (e.g. TLS configs). `merge` keeps
            # the operation idempotent and avoids duplicate PK inserts.
            session.merge(Model2Entity.kme(kme))
        session.flush()

        if not dkms_map:
            dkms_map = _build_endpoint_map(normalized_dkms)
        if not sdn_map:
            sdn_map = _build_endpoint_map(normalized_sdns)
            default_sdn_id = normalized_sdns[0].id if normalized_sdns else None
        sae_models: list[ModelSAE] = []
        for name, payload in sae_payloads:
            sae_input = _build_sae_input(payload, sdn_map, default_sdn_id, dkms_map, controllers_by_dkms)
            if sae_input is None:
                print(f"[warn] SAE {name}: no se pudo resolver DKMS/SDN/AgentController")
                continue
            tls_payload = sae_input.get("tls")
            tls_id = _persist_tls_sae(session, tls_payload) if tls_payload is not None else None
            sae_models.append(
                ModelSAE(
                    id=sae_input["id"],
                    sae_id=sae_input["sae_id"],
                    display_name=sae_input["sae_id"],
                    sdn_id=sae_input["sdn_id"],
                    tls_id=tls_id,
                    agent_dkms_id=sae_input["agent_dkms_id"],
                    owner_user_id=int(user_id),
                    simulation_id=int(simulation_id),
                    dkms_id=int(sae_input["dkms_id"]),
                    status=sae_input.get("status", SaeStatus.PENDING_CERT),
                    sdn=None,
                    tls=None,
                    agent_controller=None,
                    dkms_target=None,
                )
            )

        for sae in sae_models:
            session.merge(Model2Entity.sae(sae))
        session.flush()

        _sync_postgres_id_sequences(session)
        session.commit()

        print("Seed completado")
        print(f"User id: {user_id}")
        print(f"Simulation id: {simulation_id}")
        print(f"Hosts: {len(hosts_by_id)}")
        print(f"QKC: {len(normalized_qkcs)}")
        print(f"ORR: {len(normalized_orrs)}")
        print(f"DKMS: {len(normalized_dkms)}")
        print(f"SDN: {len(normalized_sdns)}")
        print(f"KME: {len(kme_models)}")
        print(f"AgentControllers: {len(normalized_agents)}")
        print(f"SAE: {len(sae_models)}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
