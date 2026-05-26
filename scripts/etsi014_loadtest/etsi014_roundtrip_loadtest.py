"""ETSI 014 round-trip loadtest client.

Hace lo que el dkms-loadtest:v1 NO hace: por cada request, AMBAS llamadas
ETSI 014 (master enc_keys + slave dec_keys) sobre el MISMO key_ID y
verifica que los bytes recibidos coinciden.

Topología del test (in-memory):
  - N pares (master, slave). Master y slave viven en DKMS distintos del
    sim. Cada par dispara requests a tasa Poisson λ r/s.
  - Ramp: empezamos con start_pairs pares activos, cada
    interval_seconds añadimos step_pairs hasta end_pairs.

Por cada request del par:
  1. master POST /enc_keys → (key_ID, key_master)
  2. slave  POST /dec_keys {key_IDs:[{key_ID}]} → (key_slave)
  3. registra (t_emit, key_ID, t_enc_ms, t_dec_ms, ok_enc, ok_dec,
     match, dkms_master_host_id, dkms_slave_host_id)

Salida: CSV `requests.csv` con todas las filas + `summary.json` con
agregados.

Uso:
  python3 etsi014_roundtrip_loadtest.py \\
      --sim-id 36 \\
      --orch-url http://127.0.0.1:18080 \\
      --start-pairs 5 --end-pairs 50 --step-pairs 5 \\
      --interval-seconds 20 --warmup-seconds 15 --hold-seconds 20 \\
      --lambda-rps 5.0 \\
      --out /tmp/etsi014-roundtrip
"""
from __future__ import annotations

import argparse
import asyncio
import csv
import json
import logging
import os
import pathlib
import random
import signal
import ssl
import subprocess
import sys
import time
from dataclasses import dataclass, field

try:
    import aiohttp
except ImportError:  # pragma: no cover
    print("FATAL: pip install aiohttp", file=sys.stderr)
    sys.exit(2)


log = logging.getLogger("etsi014")


# ─── data classes ────────────────────────────────────────────────────────

@dataclass
class SaePair:
    pair_id: int
    master_sae_id: str
    slave_sae_id: str
    master_host_id: int
    slave_host_id: int
    master_cert_path: pathlib.Path
    master_key_path: pathlib.Path
    slave_cert_path: pathlib.Path
    slave_key_path: pathlib.Path
    started: bool = False


@dataclass
class Record:
    t_emit: float
    pair_id: int
    master_sae: str
    slave_sae: str
    master_host: int
    slave_host: int
    key_id: str
    enc_ms: float
    dec_ms: float
    ok_enc: bool
    ok_dec: bool
    match: bool
    err: str = ""


# ─── orchestator / BD helpers ────────────────────────────────────────────

def query_db_hosts(sim_id: int) -> list[tuple[int, int]]:
    """Returns [(dkms_id, host_id), ...] for the sim.

    Para uso distribuido en pods (sin kubectl), pasar la lista via env var
    `HOSTS_JSON='[[dkms_id, host_id], ...]'`. Fallback: kubectl exec al
    orchestator pod (solo funciona desde el cliente local).
    """
    raw = os.environ.get("HOSTS_JSON")
    if raw:
        data = json.loads(raw)
        return [(int(d), int(h)) for d, h in data]
    py = f"""
import os, psycopg, urllib.parse as up
url = os.getenv('DB_URL', '').replace('postgresql://', '')
parsed = up.urlparse('postgresql://' + url)
conn = f'host={{parsed.hostname}} port={{parsed.port or 5432}} dbname={{parsed.path.lstrip(chr(47))}} user={{parsed.username}} password={{parsed.password}}'
with psycopg.connect(conn) as c, c.cursor() as cur:
    cur.execute('SELECT d.id, d.id_host FROM dkms d JOIN host h ON h.id=d.id_host WHERE h.id_simulation={sim_id} ORDER BY d.id')
    for r in cur.fetchall():
        print(f'{{r[0]}} {{r[1]}}')
"""
    out = subprocess.check_output(
        ["kubectl", "-n", "dkms-main-ns", "exec", "deploy/orchestator",
         "--", "python3", "-c", py],
        text=True,
    )
    pairs: list[tuple[int, int]] = []
    for line in out.strip().splitlines():
        a, b = line.split()
        pairs.append((int(a), int(b)))
    return pairs


def bulk_provision_saes(
    *,
    orch_url: str,
    sim_id: int,
    items: list[dict],
    out_dir: pathlib.Path,
    batch_size: int = 200,
    per_batch_timeout: float = 600.0,
) -> dict[str, dict]:
    """POST /orch/admin/saes/bulk?issue_certs=true en CHUNKS.

    Para N grandes (≥500 SAEs) el cert issuance dentro del endpoint es
    serial y puede exceder timeouts HTTP razonables. Partir en batches
    de ``batch_size`` da timeouts manejables y feedback de progreso.
    """
    import urllib.request

    bundles_map: dict[str, dict] = {}
    out_dir.mkdir(parents=True, exist_ok=True)

    total = len(items)
    n_batches = (total + batch_size - 1) // batch_size
    for bi in range(n_batches):
        chunk = items[bi * batch_size:(bi + 1) * batch_size]
        t0 = time.perf_counter()
        # Retry con backoff exponencial: con muchos workers concurrentes
        # el orchestator devuelve HTTP 500 (QueuePool SQLAlchemy lleno).
        body = None
        for attempt in range(6):
            req = urllib.request.Request(
                f"{orch_url}/orch/admin/saes/bulk?issue_certs=true&days_valid=1",
                data=json.dumps(chunk).encode("utf-8"),
                headers={"X-User-Id": "2", "Content-Type": "application/json"},
                method="POST",
            )
            try:
                with urllib.request.urlopen(req, timeout=per_batch_timeout) as resp:
                    body = json.loads(resp.read())
                break  # success
            except urllib.error.HTTPError as exc:
                if exc.code in (500, 502, 503, 504) and attempt < 5:
                    wait = (2 ** attempt) + random.uniform(0, 1)
                    log.warning("batch %d/%d HTTP %d (attempt %d/6) — retry in %.1fs",
                                bi + 1, n_batches, exc.code, attempt + 1, wait)
                    time.sleep(wait)
                    continue
                raise
            except (urllib.error.URLError, TimeoutError) as exc:
                if attempt < 5:
                    wait = (2 ** attempt) + random.uniform(0, 1)
                    log.warning("batch %d/%d %s (attempt %d/6) — retry in %.1fs",
                                bi + 1, n_batches, type(exc).__name__, attempt + 1, wait)
                    time.sleep(wait)
                    continue
                raise
        if body is None:
            log.error("batch %d/%d failed after retries — abort", bi + 1, n_batches)
            raise RuntimeError(f"bulk_provision_saes batch {bi+1}/{n_batches} failed")
        log.info("batch %d/%d (%d items): created=%s sdn_ok=%s in %.1fs",
                 bi + 1, n_batches, len(chunk),
                 body.get("created"), body.get("sdn_ok"),
                 time.perf_counter() - t0)
        if body.get("errors"):
            log.warning("  batch errors: %s", body["errors"][:5])
        for b in body.get("bundles", []):
            sae_id = b["sae_id"]
            (out_dir / f"{sae_id}.crt").write_text(b["certificate_pem"])
            (out_dir / f"{sae_id}.key").write_text(b["private_key_pem"])
            bundles_map[sae_id] = b
    return bundles_map


def get_ingress_host(sim_id: int) -> str:
    """Returns ingress host. Env override: INGRESS_HOST (para uso en pods sin kubectl)."""
    env = os.environ.get("INGRESS_HOST")
    if env:
        return env
    out = subprocess.check_output(
        ["kubectl", "-n", str(sim_id), "get", "ingress",
         "-o", "jsonpath={.items[0].spec.rules[0].host}"],
        text=True,
    ).strip()
    if not out:
        raise RuntimeError(f"no ingress host in ns {sim_id}")
    return out


# ─── per-pair driver ─────────────────────────────────────────────────────

def make_sae_ssl_context(cert: pathlib.Path, key: pathlib.Path) -> ssl.SSLContext:
    ctx = ssl.create_default_context(purpose=ssl.Purpose.SERVER_AUTH)
    ctx.load_cert_chain(certfile=str(cert), keyfile=str(key))
    # nginx ingress with auth-tls; el cert SAE va al servidor vía mTLS y
    # el servidor (nginx) usa Let's Encrypt para el lado server. El
    # default trust store ya tiene LE.
    return ctx


async def pair_driver(
    *,
    pair: SaePair,
    ingress_host: str,
    sim_id: int,
    lambda_rps: float,
    timeout_s: float,
    record_q: asyncio.Queue,
    stop_event: asyncio.Event,
    sem_in_flight: asyncio.Semaphore,
    resolver,  # aiohttp.AsyncResolver compartido
):
    """Bucle de un par: hace enc+dec a tasa Poisson.

    Pone cada Record en `record_q` (cola async-safe consumida por un
    writer task que flushea a CSV incrementalmente). Eso evita perder
    datos si el cliente se cuelga al final.
    """
    ctx_master = make_sae_ssl_context(pair.master_cert_path, pair.master_key_path)
    ctx_slave = make_sae_ssl_context(pair.slave_cert_path, pair.slave_key_path)
    conn_master = aiohttp.TCPConnector(
        ssl=ctx_master, limit=4, resolver=resolver,
        use_dns_cache=True, ttl_dns_cache=300,
    )
    conn_slave = aiohttp.TCPConnector(
        ssl=ctx_slave, limit=4, resolver=resolver,
        use_dns_cache=True, ttl_dns_cache=300,
    )
    timeout = aiohttp.ClientTimeout(total=timeout_s)

    try:
        async with aiohttp.ClientSession(connector=conn_master, timeout=timeout) as s_m, \
                aiohttp.ClientSession(connector=conn_slave, timeout=timeout) as s_s:
            url_enc = (f"https://{ingress_host}/api/sim/{sim_id}/dkms/"
                       f"{pair.master_host_id}/api/v1/keys/{pair.slave_sae_id}/enc_keys")
            url_dec = (f"https://{ingress_host}/api/sim/{sim_id}/dkms/"
                       f"{pair.slave_host_id}/api/v1/keys/{pair.master_sae_id}/dec_keys")

            # JITTER INICIAL: desfase aleatorio uniform(0, 1/λ) antes del
            # primer request → evita que todos los pares spawneados en
            # el mismo step de la rampa entren al ciclo al unísono
            # (thundering herd auto-organizado observado en plots con
            # patrón pulsante 0↔pico).
            inter_arrival_mean = 1.0 / lambda_rps
            await asyncio.sleep(random.uniform(0, inter_arrival_mean))

            while not stop_event.is_set():
                # INTER-ARRIVAL UNIFORM en lugar de exponential: misma
                # media (1/λ) pero menos varianza → menos colas largas
                # y menos probabilidad de que pares converjan al mismo
                # instante tras una respuesta común. 2 × mean da
                # rango [0, 2/λ] con media 1/λ.
                await asyncio.sleep(random.uniform(0, 2 * inter_arrival_mean))
                if stop_event.is_set():
                    break
                await sem_in_flight.acquire()
                try:
                    rec = await _do_one_roundtrip(
                        s_m=s_m, s_s=s_s,
                        url_enc=url_enc, url_dec=url_dec,
                        pair=pair,
                    )
                    record_q.put_nowait(rec)
                finally:
                    sem_in_flight.release()
    except asyncio.CancelledError:
        # En cancel duro al final, salir limpio sin errores.
        pass


async def csv_writer_task(
    csv_path: pathlib.Path,
    record_q: asyncio.Queue,
    stop_event: asyncio.Event,
    fields: list[str],
):
    """Writer task que drena `record_q` y escribe filas al CSV cada
    flush_interval. Append mode, no trunca. Termina cuando stop_event
    está set Y la cola está vacía.
    """
    csv_path.parent.mkdir(parents=True, exist_ok=True)
    # Header solo si el archivo no existe (no perdemos datos en restart).
    write_header = not csv_path.exists() or csv_path.stat().st_size == 0
    fp = csv_path.open("a", buffering=1)  # line-buffered
    w = csv.writer(fp)
    if write_header:
        w.writerow(fields)
        fp.flush()
    written = 0
    try:
        while True:
            try:
                rec = await asyncio.wait_for(record_q.get(), timeout=1.0)
            except asyncio.TimeoutError:
                if stop_event.is_set() and record_q.empty():
                    break
                continue
            w.writerow([
                f"{rec.t_emit:.6f}", rec.pair_id, rec.master_sae, rec.slave_sae,
                rec.master_host, rec.slave_host, rec.key_id,
                f"{rec.enc_ms:.2f}", f"{rec.dec_ms:.2f}",
                int(rec.ok_enc), int(rec.ok_dec), int(rec.match), rec.err,
            ])
            written += 1
            if written % 1000 == 0:
                fp.flush()
        fp.flush()
    finally:
        fp.close()
    log.info("csv_writer: wrote %d records to %s", written, csv_path)


async def _do_one_roundtrip(
    *,
    s_m: aiohttp.ClientSession,
    s_s: aiohttp.ClientSession,
    url_enc: str,
    url_dec: str,
    pair: SaePair,
) -> Record:
    t0 = time.perf_counter()
    t_emit = time.time()
    ok_enc = False
    ok_dec = False
    match = False
    key_id = ""
    enc_ms = 0.0
    dec_ms = 0.0
    err = ""

    # 1. enc_keys.
    try:
        t_enc_start = time.perf_counter()
        async with s_m.post(url_enc, json={"number": 1, "size": 256}) as resp:
            enc_ms = (time.perf_counter() - t_enc_start) * 1000.0
            if resp.status != 200:
                err = f"enc HTTP {resp.status}"
                txt = await resp.text()
                err += f": {txt[:120]}"
                return Record(t_emit, pair.pair_id, pair.master_sae_id, pair.slave_sae_id,
                              pair.master_host_id, pair.slave_host_id, "",
                              enc_ms, 0.0, False, False, False, err)
            body = await resp.json()
            key_id = body["keys"][0]["key_ID"]
            key_master = body["keys"][0]["key"]
            ok_enc = True
    except Exception as exc:  # noqa: BLE001
        return Record(t_emit, pair.pair_id, pair.master_sae_id, pair.slave_sae_id,
                      pair.master_host_id, pair.slave_host_id, "",
                      enc_ms, 0.0, False, False, False, f"enc exc: {exc}")

    # 2. dec_keys.
    try:
        t_dec_start = time.perf_counter()
        async with s_s.post(url_dec, json={"key_IDs": [{"key_ID": key_id}]}) as resp:
            dec_ms = (time.perf_counter() - t_dec_start) * 1000.0
            if resp.status != 200:
                err = f"dec HTTP {resp.status}"
                txt = await resp.text()
                err += f": {txt[:120]}"
                return Record(t_emit, pair.pair_id, pair.master_sae_id, pair.slave_sae_id,
                              pair.master_host_id, pair.slave_host_id, key_id,
                              enc_ms, dec_ms, ok_enc, False, False, err)
            body = await resp.json()
            key_slave = body["keys"][0]["key"]
            ok_dec = True
            match = (key_master == key_slave)
    except Exception as exc:  # noqa: BLE001
        return Record(t_emit, pair.pair_id, pair.master_sae_id, pair.slave_sae_id,
                      pair.master_host_id, pair.slave_host_id, key_id,
                      enc_ms, dec_ms, ok_enc, False, False, f"dec exc: {exc}")

    return Record(t_emit, pair.pair_id, pair.master_sae_id, pair.slave_sae_id,
                  pair.master_host_id, pair.slave_host_id, key_id,
                  enc_ms, dec_ms, ok_enc, ok_dec, match, err)


# ─── main ────────────────────────────────────────────────────────────────

async def main_async(args: argparse.Namespace) -> int:
    out = pathlib.Path(args.out)
    out.mkdir(parents=True, exist_ok=True)
    certs_dir = out / "certs"

    # 1. Resolver DKMS hosts del sim.
    hosts = query_db_hosts(args.sim_id)
    log.info("sim %s has %d DKMS hosts: %s", args.sim_id, len(hosts), hosts)
    if len(hosts) < 2:
        log.fatal("need >=2 DKMS to make pairs")
        return 2

    # 2. Provisionar SAEs en bulk. Cada par necesita 2 SAEs en distintos
    #    DKMS. Para evitar sesgo, distribuimos los pares uniformemente
    #    sobre los DKMS.
    n_pairs = args.end_pairs
    items: list[dict] = []
    pair_assignments: list[tuple[int, int]] = []  # (master_dkms_id, slave_dkms_id) for pair i
    # Tag compartido entre todos los workers de un mismo test (se pasa
    # via env SHARE_TAG cuando se lanza en cluster distribuido). El
    # worker_id evita colisiones de SAE ids entre pods.
    share_tag = args.share_tag or str(int(time.time()))
    wid = args.worker_id
    log.info("worker_id=%d share_tag=%s", wid, share_tag)
    for i in range(n_pairs):
        # Round-robin distribution sobre los DKMS, con offset por worker
        # para evitar que todos los workers concentren la misma carga
        # en el mismo par master/slave.
        idx_m = (i + wid) % len(hosts)
        idx_s = (i + wid + 1) % len(hosts)
        m_dkms_id, m_host_id = hosts[idx_m]
        s_dkms_id, s_host_id = hosts[idx_s]
        if m_dkms_id == s_dkms_id:
            s_dkms_id, s_host_id = hosts[(idx_s + 1) % len(hosts)]
        m_sae = f"rt-{share_tag}-w{wid:02d}-m-{i:04d}"
        s_sae = f"rt-{share_tag}-w{wid:02d}-s-{i:04d}"
        items.append({
            "simulation_id": args.sim_id,
            "dkms_id": m_dkms_id,
            "sae_id": m_sae,
            "display_name": m_sae,
        })
        items.append({
            "simulation_id": args.sim_id,
            "dkms_id": s_dkms_id,
            "sae_id": s_sae,
            "display_name": s_sae,
        })
        pair_assignments.append((m_host_id, s_host_id))

    log.info("provisioning %d SAEs (%d pairs) via bulk endpoint", len(items), n_pairs)
    t_p = time.perf_counter()
    bundles = bulk_provision_saes(
        orch_url=args.orch_url,
        sim_id=args.sim_id,
        items=items,
        out_dir=certs_dir,
    )
    log.info("provisioned in %.1fs", time.perf_counter() - t_p)

    pairs: list[SaePair] = []
    for i in range(n_pairs):
        m_sae = f"rt-{share_tag}-w{wid:02d}-m-{i:04d}"
        s_sae = f"rt-{share_tag}-w{wid:02d}-s-{i:04d}"
        if m_sae not in bundles or s_sae not in bundles:
            log.warning("missing bundle for pair %d", i)
            continue
        m_host_id, s_host_id = pair_assignments[i]
        pairs.append(SaePair(
            pair_id=i,
            master_sae_id=m_sae,
            slave_sae_id=s_sae,
            master_host_id=m_host_id,
            slave_host_id=s_host_id,
            master_cert_path=certs_dir / f"{m_sae}.crt",
            master_key_path=certs_dir / f"{m_sae}.key",
            slave_cert_path=certs_dir / f"{s_sae}.crt",
            slave_key_path=certs_dir / f"{s_sae}.key",
        ))
    log.info("ready %d pairs", len(pairs))

    # 3. Ingress host.
    ingress_host = get_ingress_host(args.sim_id)
    log.info("ingress host=%s", ingress_host)

    # 3.5. Resolver async compartido: evita el cuello del threadpool de
    # getaddrinfo cuando hay cientos de pares activos. Sin esto, a
    # >250 pares concurrentes el cliente acumula "Cannot connect to
    # host" antes incluso de enviar la request.
    try:
        resolver = aiohttp.AsyncResolver()
        log.info("resolver=AsyncResolver (aiodns)")
    except RuntimeError as exc:
        log.warning("AsyncResolver unavailable (%s) — falling back to ThreadedResolver", exc)
        resolver = aiohttp.ThreadedResolver()

    # 4. Bucle de ramp.
    csv_path = out / "requests.csv"
    fields = ["t_emit", "pair_id", "master_sae", "slave_sae",
              "master_host_id", "slave_host_id", "key_id",
              "enc_ms", "dec_ms", "ok_enc", "ok_dec", "match", "err"]
    record_q: asyncio.Queue = asyncio.Queue()
    stop_event = asyncio.Event()
    sem = asyncio.Semaphore(args.max_in_flight)
    tasks: list[asyncio.Task] = []
    t_start = time.time()

    # SIGTERM handler: doble función:
    # 1. Durante ramp/hold: setea stop_event → pair_drivers paran.
    # 2. Después de DONE: setea shutdown_event → main exit 0.
    # Necesitamos QUEDARNOS running después de DONE porque kubectl cp no
    # funciona en Pods Succeeded (exec requiere container running).
    loop = asyncio.get_running_loop()
    shutdown_event = asyncio.Event()
    sigterm_seen = False
    def _on_sigterm():
        nonlocal sigterm_seen
        if sigterm_seen:
            return
        sigterm_seen = True
        log.warning("SIGTERM received — initiating graceful shutdown")
        stop_event.set()
        shutdown_event.set()
    loop.add_signal_handler(signal.SIGTERM, _on_sigterm)
    loop.add_signal_handler(signal.SIGINT, _on_sigterm)

    # Writer task que drena record_q incrementalmente al CSV (append).
    writer_task = asyncio.create_task(csv_writer_task(csv_path, record_q, stop_event, fields))

    log.info("warmup %.1fs", args.warmup_seconds)
    await asyncio.sleep(args.warmup_seconds)

    if args.start_at_ts > 0:
        wait_s = args.start_at_ts - time.time()
        if wait_s > 0:
            log.info("sync: waiting %.1fs until start_at_ts=%.0f", wait_s, args.start_at_ts)
            await asyncio.sleep(wait_s)
        else:
            log.warning("sync: start_at_ts already past (%.1fs ago) — starting now", -wait_s)

    next_pair_idx = 0
    target = args.start_pairs
    log.info("ramp start_pairs=%d end_pairs=%d step=%d interval=%.1fs",
             args.start_pairs, args.end_pairs, args.step_pairs,
             args.interval_seconds)

    while target <= args.end_pairs:
        while next_pair_idx < target and next_pair_idx < len(pairs):
            p = pairs[next_pair_idx]
            t = asyncio.create_task(pair_driver(
                pair=p, ingress_host=ingress_host, sim_id=args.sim_id,
                lambda_rps=args.lambda_rps,
                timeout_s=args.request_timeout,
                record_q=record_q, stop_event=stop_event,
                sem_in_flight=sem,
                resolver=resolver,
            ))
            tasks.append(t)
            next_pair_idx += 1
        log.info("step → %d pairs active", target)
        if target == args.end_pairs:
            break
        await asyncio.sleep(args.interval_seconds)
        target = min(target + args.step_pairs, args.end_pairs)

    log.info("hold %.1fs with %d pairs", args.hold_seconds, next_pair_idx)
    await asyncio.sleep(args.hold_seconds)
    log.info("stop signal → cancel pair drivers")
    stop_event.set()

    # CANCEL DURO: en lugar de await asyncio.wait(tasks, timeout=30) que
    # puede colgarse si alguna task está bloqueada en s.post() bajo
    # backpressure profunda, cancel explícitamente y luego gather con
    # return_exceptions=True para no propagar la CancelledError.
    for t in tasks:
        t.cancel()
    try:
        await asyncio.wait_for(
            asyncio.gather(*tasks, return_exceptions=True),
            timeout=15.0,
        )
    except asyncio.TimeoutError:
        log.warning("some pair drivers did not cancel within 15s — forcing exit")

    # Cerrar writer (drena lo que quede en cola).
    log.info("flushing CSV writer")
    try:
        await asyncio.wait_for(writer_task, timeout=10.0)
    except asyncio.TimeoutError:
        log.warning("writer task timeout — cancelling")
        writer_task.cancel()

    # 5. Leer CSV para summary (lo escribió incrementalmente el writer).
    n = n_ok_enc = n_ok_dec = n_match = 0
    enc_lats = []
    dec_lats = []
    if csv_path.exists():
        with csv_path.open() as fp:
            r = csv.DictReader(fp)
            for row in r:
                n += 1
                if row["ok_enc"] == "1":
                    n_ok_enc += 1
                    enc_lats.append(float(row["enc_ms"]))
                if row["ok_dec"] == "1":
                    n_ok_dec += 1
                    dec_lats.append(float(row["dec_ms"]))
                if row["match"] == "1":
                    n_match += 1
    enc_lats.sort()
    dec_lats.sort()
    elapsed = time.time() - t_start
    summary = {
        "test_id": f"etsi014-roundtrip-{share_tag}-w{wid:02d}",
        "sim_id": args.sim_id,
        "n_pairs_total": args.end_pairs,
        "lambda_rps": args.lambda_rps,
        "ramp_start": args.start_pairs,
        "ramp_end": args.end_pairs,
        "ramp_step": args.step_pairs,
        "interval_seconds": args.interval_seconds,
        "hold_seconds": args.hold_seconds,
        "total_elapsed_seconds": round(elapsed, 1),
        "total_requests": n,
        "enc_ok": n_ok_enc,
        "dec_ok": n_ok_dec,
        "match": n_match,
        "enc_ok_pct": round(100.0 * n_ok_enc / n, 2) if n else 0,
        "dec_ok_pct": round(100.0 * n_ok_dec / n, 2) if n else 0,
        "match_pct": round(100.0 * n_match / n, 2) if n else 0,
        "enc_latency_ms": _pcts(enc_lats),
        "dec_latency_ms": _pcts(dec_lats),
    }
    (out / "summary.json").write_text(json.dumps(summary, indent=2))
    log.info("wrote %s + summary", csv_path)
    print(json.dumps(summary, indent=2))
    # Marcador para que el bash externo detecte finalización fiable (kubectl exec sh -c '[ -f DONE ]').
    (out / "DONE").write_text(f"{int(time.time())}\n")
    log.info("DONE marker written — waiting for SIGTERM (kubectl cp needs Pod running)")
    # Esperamos SIGTERM externo: el bash hace kubectl cp mientras Pod
    # Running (sin esto, Pod en Succeeded → exec/cp falla con
    # "cannot exec into a container in a completed pod"). Después, el
    # bash hace `kubectl delete jobs` → SIGTERM → shutdown_event.set →
    # exit 0 clean → Pod Succeeded sin re-spawn (semántica Job).
    await shutdown_event.wait()
    log.info("shutdown signal received — exit 0")
    return 0


def _pcts(lats: list[float]) -> dict:
    if not lats:
        return {}
    return {
        "p50": round(lats[len(lats) // 2], 2),
        "p95": round(lats[int(len(lats) * 0.95)], 2),
        "p99": round(lats[int(len(lats) * 0.99)], 2),
        "max": round(lats[-1], 2),
    }


def parse_args() -> argparse.Namespace:
    """CLI args con fallback a env vars (necesario para uso en pods)."""
    def env_or(name, default):
        v = os.environ.get(name)
        return v if v is not None else default

    p = argparse.ArgumentParser()
    p.add_argument("--sim-id", type=int, default=int(env_or("SIM_ID", 0)) or None, required=False)
    p.add_argument("--orch-url", default=env_or("ORCH_URL", "http://127.0.0.1:18080"))
    p.add_argument("--start-pairs", type=int, default=int(env_or("START_PAIRS", 5)))
    p.add_argument("--end-pairs", type=int, default=int(env_or("END_PAIRS", 50)))
    p.add_argument("--step-pairs", type=int, default=int(env_or("STEP_PAIRS", 5)))
    p.add_argument("--interval-seconds", type=float, default=float(env_or("INTERVAL_SECONDS", 20.0)))
    p.add_argument("--warmup-seconds", type=float, default=float(env_or("WARMUP_SECONDS", 15.0)))
    p.add_argument("--hold-seconds", type=float, default=float(env_or("HOLD_SECONDS", 20.0)))
    p.add_argument("--lambda-rps", type=float, default=float(env_or("LAMBDA_RPS", 5.0)))
    p.add_argument("--request-timeout", type=float, default=float(env_or("REQUEST_TIMEOUT", 20.0)))
    p.add_argument("--max-in-flight", type=int, default=int(env_or("MAX_IN_FLIGHT", 2000)),
                   help="semaphore cap to avoid sockets explosion")
    p.add_argument("--out", default=env_or("OUT_DIR", "/tmp/etsi014-out"))
    p.add_argument("--worker-id", type=int, default=int(env_or("WORKER_ID", 0)))
    p.add_argument("--share-tag", default=env_or("SHARE_TAG", None),
                   help="timestamp/tag shared across workers of same test (default: current time)")
    p.add_argument("--start-at-ts", type=float, default=float(env_or("START_AT_TS", 0)),
                   help="unix timestamp to start the ramp (after warmup); 0 = start immediately")
    args = p.parse_args()
    if not args.sim_id:
        raise SystemExit("--sim-id (o env SIM_ID) requerido")
    return args


if __name__ == "__main__":
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)-7s %(name)s: %(message)s",
        stream=sys.stderr,
    )
    args = parse_args()
    raise SystemExit(asyncio.run(main_async(args)))
