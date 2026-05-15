from __future__ import annotations

import argparse
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

from sqlalchemy import create_engine
from sqlalchemy.orm import sessionmaker

from dotenv import load_dotenv

load_dotenv()

ROOT = Path(__file__).resolve().parents[2]
if str(ROOT) not in sys.path:
    sys.path.append(str(ROOT))

from models import (  # noqa: E402
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
    SimulationStatus,
    TLSConfigDKMS,
    TLSConfigSAE,
    TLSVersion,
    TokenBucketConfig,
)
from models.enums import HTTPType  # noqa: E402
from persistence.sqlalchemy.data import (  # noqa: E402
    AgentController,
    Base,
    DKMS,
    KME,
    ORR,
    QKC,
    SAE,
    SDN,
    Simulation,
)
from persistence.sqlalchemy.mappers import Entity2Model, Model2Entity  # noqa: E402


def _add_and_refresh(session, entity):
    session.add(entity)
    session.flush()
    session.refresh(entity)
    return entity


def _merge_and_refresh(session, entity):
    merged = session.merge(entity)
    session.flush()
    session.refresh(merged)
    return merged


def _check(condition: bool, message: str, errors: list[str]) -> None:
    if not condition:
        errors.append(message)


def _persist_host(session, model: ModelHost) -> ModelHost:
    entity = Model2Entity.host(model)
    entity = _add_and_refresh(session, entity)
    return model.model_copy(update={"id": entity.id})


def run_smoke_test(session) -> list[str]:
    errors: list[str] = []

    user_model = ModelUser(
        username="test_user",
        email="test_user@example.com",
        password_hash="hash",
        is_active=True,
    )
    user_entity = _add_and_refresh(session, Model2Entity.user(user_model))
    user_model = user_model.model_copy(update={"id": user_entity.id})

    placeholder_sdn = ModelSDN(id=None, id_host=None, host=None, type_http=HTTPType.HTTPS)
    simulation_model = ModelSimulation(
        id_user=user_entity.id,
        name="sim_test",
        description="smoke test",
        start_time=datetime.now(timezone.utc),
        status=SimulationStatus.PENDING,
        user=user_model,
        list_dkms=[],
        sdn=placeholder_sdn,
    )
    simulation_entity = _add_and_refresh(session, Model2Entity.simulation(simulation_model))
    simulation_model = simulation_model.model_copy(update={"id": simulation_entity.id})

    host_qkc_local = _persist_host(
        session,
        ModelHost(id_simulation=simulation_entity.id, ip="10.0.0.1", port=5001),
    )
    host_qkc_neighbor = _persist_host(
        session,
        ModelHost(id_simulation=simulation_entity.id, ip="10.0.0.2", port=5002),
    )
    host_orr = _persist_host(
        session,
        ModelHost(id_simulation=simulation_entity.id, ip="10.0.0.3", port=6001),
    )
    host_dkms = _persist_host(
        session,
        ModelHost(id_simulation=simulation_entity.id, ip="10.0.0.4", port=7001),
    )
    host_sdn = _persist_host(
        session,
        ModelHost(id_simulation=simulation_entity.id, ip="10.0.0.5", port=8001),
    )

    qkc_neighbor_model = ModelQKC(
        id_host=host_qkc_neighbor.id,
        kme_host="neighbor-kme",
        host=host_qkc_neighbor,
        kmes=[],
    )
    qkc_neighbor_entity = _merge_and_refresh(session, Model2Entity.qkc(qkc_neighbor_model))
    qkc_neighbor_model = qkc_neighbor_model.model_copy(update={"id": qkc_neighbor_entity.id})

    qkc_local_model = ModelQKC(
        id_host=host_qkc_local.id,
        kme_host="local-kme",
        host=host_qkc_local,
        kmes=[],
    )
    qkc_local_entity = _merge_and_refresh(session, Model2Entity.qkc(qkc_local_model))
    qkc_local_model = qkc_local_model.model_copy(update={"id": qkc_local_entity.id})

    kme_config = KMEConfig(
        id=None,
        local_qkc_id=qkc_local_model.id,
        local_qkc_ip=host_qkc_local.ip,
        neighbor_qkc_id=qkc_neighbor_model.id,
        neighbor_qkc_ip=host_qkc_neighbor.ip,
        neighbor_qkc_port=host_qkc_neighbor.port,
        neighbor_qkd_id="NEI_QKD",
        local_qkd_id=str(qkc_local_model.id),
        local_url_node_qkd="https://kme.local",
        etsi=ETSIType.ETSI_004,
        cert=ModelFile(path="certs/kme_local.crt"),
        key=ModelFile(path="certs/kme_local.key"),
        token_bucket=TokenBucketConfig(beta=0.1, gamma=0.2, delta_up=0.3),
        channel=Channel(type_channel=ChannelType.QKD, distance=5),
    )
    qkc_local_with_kme = qkc_local_model.model_copy(update={"kmes": [kme_config]})
    _merge_and_refresh(session, Model2Entity.qkc(qkc_local_with_kme))

    orr_model = ModelORR(
        id_host=host_orr.id,
        qkc_id=qkc_local_model.id,
        host=host_orr,
        qkc=qkc_local_with_kme,
    )
    orr_entity = _merge_and_refresh(session, Model2Entity.orr(orr_model))
    orr_model = orr_model.model_copy(update={"id": orr_entity.id})

    tls_dkms_model = TLSConfigDKMS(
        cert=ModelFile(path="certs/dkms.crt"),
        key=ModelFile(path="certs/dkms.key"),
        ca_cert=ModelFile(path="certs/ca.crt"),
        require_client_cert=True,
        version=TLSVersion.TLSV1_3,
        ciphers=[
            CipherDKMS.TLS_AES_256_GCM_SHA384,
            CipherDKMS.TLS_AES_128_GCM_SHA256,
        ],
    )
    tls_dkms_entity = _add_and_refresh(session, Model2Entity.tls_dkms(tls_dkms_model))
    tls_dkms_model = tls_dkms_model.model_copy(update={"id": tls_dkms_entity.id})

    dkms_model = ModelDKMS(
        id_host=host_dkms.id,
        orr_id=orr_model.id,
        tls_id=tls_dkms_model.id,
        host=host_dkms,
        orr=orr_model,
        tls=tls_dkms_model,
    )
    dkms_entity = _merge_and_refresh(session, Model2Entity.dkms(dkms_model))
    dkms_model = dkms_model.model_copy(update={"id": dkms_entity.id})

    sdn_model = ModelSDN(
        id_host=host_sdn.id,
        host=host_sdn,
        type_http=HTTPType.HTTPS,
    )
    sdn_entity = _merge_and_refresh(session, Model2Entity.sdn(sdn_model))
    sdn_model = sdn_model.model_copy(update={"id": sdn_entity.id})

    agent_model = ModelAgentController(
        id_dkms=dkms_model.id,
        id_sdn=sdn_model.id,
        id_host=host_dkms.id,
        sdn=sdn_model,
        host=host_dkms,
    )
    agent_entity = _merge_and_refresh(session, Model2Entity.agent_controller(agent_model))
    agent_model = agent_model.model_copy(update={"id": agent_entity.id})

    tls_sae_model = TLSConfigSAE(
        cert=ModelFile(path="certs/sae.crt"),
        key=ModelFile(path="certs/sae.key"),
        ca_certs=ModelFile(path="certs/sae_ca.crt"),
        use_client_cert=False,
    )
    tls_sae_entity = _add_and_refresh(session, Model2Entity.tls_sae(tls_sae_model))
    tls_sae_model = tls_sae_model.model_copy(update={"id": tls_sae_entity.id})

    sae_model = ModelSAE(
        sdn_id=sdn_model.id,
        tls_id=tls_sae_model.id,
        agent_dkms_id=agent_model.id,
        sdn=sdn_model,
        tls=tls_sae_model,
        agent_controller=agent_model,
        dkms_target=None,
    )
    sae_entity = _merge_and_refresh(session, Model2Entity.sae(sae_model))

    qkc_loaded = Entity2Model.qkc(session.get(QKC, qkc_local_model.id))
    _check(qkc_loaded is not None, "QKC not loaded", errors)
    if qkc_loaded and qkc_loaded.kmes:
        kme_loaded = qkc_loaded.kmes[0]
        _check(kme_loaded.local_qkc_id == qkc_local_model.id, "KME.local_qkc_id mismatch", errors)
        _check(kme_loaded.neighbor_qkc_id == qkc_neighbor_model.id, "KME.neighbor_qkc_id mismatch", errors)
        _check(kme_loaded.local_qkc_ip == host_qkc_local.ip, "KME.local_qkc_ip mismatch", errors)
        _check(kme_loaded.neighbor_qkc_ip == host_qkc_neighbor.ip, "KME.neighbor_qkc_ip mismatch", errors)
        _check(
            kme_loaded.neighbor_qkc_port == host_qkc_neighbor.port,
            "KME.neighbor_qkc_port mismatch",
            errors,
        )
        _check(
            kme_loaded.local_url_node_qkd == kme_config.local_url_node_qkd,
            "KME.local_url_node_qkd mismatch",
            errors,
        )
        _check(
            kme_loaded.neighbor_qkd_id == kme_config.neighbor_qkd_id,
            "KME.neighbor_qkd_id mismatch",
            errors,
        )
        _check(
            kme_loaded.local_qkd_id == str(qkc_local_model.id),
            "KME.local_qkd_id mismatch",
            errors,
        )
    else:
        errors.append("QKC has no KME entries")

    dkms_loaded = Entity2Model.dkms(session.get(DKMS, dkms_model.id))
    _check(dkms_loaded is not None, "DKMS not loaded", errors)
    if dkms_loaded and dkms_loaded.orr:
        _check(
            dkms_loaded.orr.qkc_id == qkc_local_model.id,
            "DKMS.ORR.QKC id mismatch",
            errors,
        )
    else:
        errors.append("DKMS missing ORR relation")

    sdn_loaded = Entity2Model.sdn(session.get(SDN, sdn_model.id))
    _check(sdn_loaded is not None, "SDN not loaded", errors)
    if sdn_loaded:
        _check(sdn_loaded.type_http == HTTPType.HTTPS, "SDN.type_http mismatch", errors)

    sae_loaded = Entity2Model.sae(session.get(SAE, sae_entity.id))
    _check(sae_loaded is not None, "SAE not loaded", errors)
    if sae_loaded:
        _check(
            sae_loaded.agent_dkms_id == agent_model.id,
            "SAE.agent_dkms_id mismatch",
            errors,
        )

    sim_loaded = Entity2Model.simulation(session.get(Simulation, simulation_entity.id))
    _check(sim_loaded is not None, "Simulation not loaded", errors)
    if sim_loaded:
        _check(len(sim_loaded.list_dkms) == 1, "Simulation.list_dkms size mismatch", errors)
        _check(sim_loaded.sdn is not None, "Simulation.sdn missing", errors)
        if sim_loaded.sdn:
            _check(sim_loaded.sdn.id == sdn_model.id, "Simulation.sdn id mismatch", errors)

    _check(session.get(ORR, orr_entity.id) is not None, "ORR missing", errors)
    _check(session.get(KME, kme_loaded.id if qkc_loaded and qkc_loaded.kmes else None) is not None, "KME missing", errors)
    _check(
        session.get(AgentController, agent_entity.id) is not None,
        "AgentController missing",
        errors,
    )

    return errors


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Smoke test for SQLAlchemy mappings.")
    parser.add_argument(
        "--db-url",
        dest="db_url",
        default=os.getenv("DB_URL") or os.getenv("DATABASE_URL"),
        help="Database URL. Defaults to DB_URL or DATABASE_URL.",
    )
    parser.add_argument(
        "--create-schema",
        action="store_true",
        help="Create tables using SQLAlchemy metadata if missing.",
    )
    parser.add_argument(
        "--commit",
        action="store_true",
        help="Commit the test data instead of rolling back.",
    )
    return parser.parse_args()


def main() -> int:
    args = _parse_args()


    db_url = args.db_url or os.getenv("DATABASE_URL") or os.getenv("DB_URL")

    if not db_url:
        print("DB_URL not set. Use --db-url or export DB_URL/DATABASE_URL.")
        return 2

    engine = create_engine(db_url)
    if args.create_schema:
        Base.metadata.create_all(engine)

    SessionLocal = sessionmaker(autocommit=False, autoflush=False, bind=engine)
    session = SessionLocal()
    try:
        errors = run_smoke_test(session)
        if errors:
            print("FAIL")
            for err in errors:
                print(f"- {err}")
            session.rollback()
            return 1
        if args.commit:
            session.commit()
            print("OK (committed)")
        else:
            session.rollback()
            print("OK (rolled back)")
        return 0
    except Exception as exc:
        session.rollback()
        print(f"ERROR: {exc}")
        return 1
    finally:
        session.close()


if __name__ == "__main__":
    raise SystemExit(main())
