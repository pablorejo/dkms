#!/usr/bin/env python3
"""Cliente SAE de carga — corre EN la VM del nodo, contra su DKMS local.

Un SAE real vive junto a su DKMS, así que la carga se genera ahí y no desde
el portátil: así lo que se mide es la cadena DKMS→ORR→QKC y no la red del
laboratorio.

Cada hilo mantiene su propia conexión HTTPS con keep-alive (mTLS) y pide
`enc_keys` en bucle. Por petición escribe una línea CSV:

    t_unix,thread,status,latency_ms,n_keys,key_id

`status` es el código HTTP, o `ERR:<tipo>` si la petición ni llegó a responder.

Verificación de integridad bajo carga: con --record-keys se anota además
`key_id,sha256(key)` en un fichero aparte, para que el orquestador tome una
muestra y compruebe en el DKMS del esclavo que los bytes coinciden. Es la
única forma de detectar que dos claves de transporte se han desincronizado:
la clave de sesión del SAE no lleva integridad propia.

    ./sae_load.py --sae sae_1 --slave sae_2 --certs /home/debian/site/certs \
                  --threads 16 --duration 300 --out /tmp/load_sae_1.csv
"""
from __future__ import annotations

import argparse
import hashlib
import json
import ssl
import sys
import threading
import time
from http.client import HTTPSConnection
from queue import Queue, Empty


def make_ctx(certs: str, sae: str) -> ssl.SSLContext:
    # El cert de servidor del DKMS lo firma la misma CA de test, cuyo SAN es
    # IP + DNS:localhost. Conectamos por 127.0.0.1 y verificamos contra la CA;
    # si el SAN no cubriera localhost, esto fallaría de forma ruidosa — que es
    # lo que queremos, no un fallo silencioso.
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False           # el SAN lleva IP, no hostname
    ctx.verify_mode = ssl.CERT_REQUIRED
    ctx.load_verify_locations(f"{certs}/ca.crt")
    ctx.load_cert_chain(certfile=f"{certs}/{sae}.crt", keyfile=f"{certs}/{sae}.key")
    return ctx


def worker(idx: int, args, ctx: ssl.SSLContext, stop: threading.Event,
           rows: Queue, keys: Queue) -> None:
    path = f"/api/v1/keys/{args.slave}/enc_keys"
    body = json.dumps({"number": args.number, "size": args.size})
    hdrs = {"Content-Type": "application/json"}
    conn: HTTPSConnection | None = None
    min_period = 1.0 / args.rate_cap if args.rate_cap > 0 else 0.0
    throttled: dict[str, int] = {}
    throttled_second = [int(time.time())]

    while not stop.is_set():
        cycle_start = time.time()
        t0 = time.perf_counter()
        try:
            if conn is None:
                conn = HTTPSConnection(args.host, args.port, context=ctx,
                                       timeout=args.timeout)
            conn.request("POST", path, body=body, headers=hdrs)
            resp = conn.getresponse()
            payload = resp.read()
            dt = (time.perf_counter() - t0) * 1000.0
            status = str(resp.status)
            n_keys, key_id = 0, ""
            if resp.status == 200:
                try:
                    doc = json.loads(payload)
                    ks = doc.get("keys", [])
                    n_keys = len(ks)
                    if ks:
                        key_id = ks[0].get("key_ID", "")
                        if args.record_keys:
                            for k in ks:
                                digest = hashlib.sha256(
                                    k.get("key", "").encode()).hexdigest()
                                keys.put(f"{k.get('key_ID','')},{digest}")
                except json.JSONDecodeError:
                    status = "ERR:badjson"
            # Una respuesta sin keep-alive obliga a reconectar en la siguiente.
            if resp.will_close:
                conn.close(); conn = None
        except Exception as exc:                      # red, TLS, timeout
            dt = (time.perf_counter() - t0) * 1000.0
            status = f"ERR:{type(exc).__name__}"
            n_keys, key_id = 0, ""
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass
                conn = None
            # Sin esta pausa, un DKMS caído convierte el test en un bucle
            # ocupado que mide la velocidad del bucle, no la del sistema.
            time.sleep(0.05)

        # Una fila por petición rechazada no aporta nada y sí mucho volumen:
        # bajo saturación el servidor devuelve 429 a miles por segundo y el CSV
        # se va a cientos de MB por punto del barrido, que luego hay que copiar
        # por scp. Con --aggregate-throttled solo se cuentan, y se emite una
        # fila de resumen por segundo con `thread=-1`.
        if args.aggregate_throttled and status in ("429", "503"):
            throttled[status] = throttled.get(status, 0) + 1
            now_s = int(time.time())
            if now_s != throttled_second[0]:
                for st, n in throttled.items():
                    rows.put(f"{throttled_second[0]}.000,-1,{st},0.00,0,x{n}")
                throttled.clear()
                throttled_second[0] = now_s
        else:
            rows.put(f"{time.time():.3f},{idx},{status},{dt:.2f},{n_keys},{key_id}")

        if min_period:
            sleep = min_period - (time.time() - cycle_start)
            if sleep > 0:
                time.sleep(sleep)

    if conn is not None:
        try:
            conn.close()
        except Exception:
            pass


def drain(q: Queue, fh) -> int:
    n = 0
    while True:
        try:
            fh.write(q.get_nowait() + "\n")
            n += 1
        except Empty:
            return n


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--sae", required=True, help="id del SAE maestro (cert de cliente)")
    p.add_argument("--slave", required=True, help="id del SAE destino")
    p.add_argument("--certs", default="/home/debian/site/certs")
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=20005)
    p.add_argument("--threads", type=int, default=1)
    p.add_argument("--duration", type=float, default=60.0)
    p.add_argument("--number", type=int, default=1, help="claves por petición")
    p.add_argument("--size", type=int, default=256, help="bits por clave")
    p.add_argument("--rate-cap", type=float, default=0.0,
                   help="límite de peticiones/s por hilo (0 = sin límite)")
    p.add_argument("--timeout", type=float, default=20.0)
    p.add_argument("--out", required=True, help="CSV de salida")
    p.add_argument("--record-keys", default="",
                   help="fichero key_id,sha256 para la verificación por muestreo")
    p.add_argument("--aggregate-throttled", action="store_true",
                   help="cuenta los 429/503 por segundo en vez de una fila por "
                        "petición (evita CSV de cientos de MB bajo saturación)")
    args = p.parse_args()
    args.record_keys = args.record_keys or None

    ctx = make_ctx(args.certs, args.sae)
    stop = threading.Event()
    rows: Queue = Queue()
    keys: Queue = Queue()

    threads = [threading.Thread(target=worker, args=(i, args, ctx, stop, rows, keys),
                                daemon=True)
               for i in range(args.threads)]

    fh = open(args.out, "w", buffering=1)
    fh.write("t_unix,thread,status,latency_ms,n_keys,key_id\n")
    kh = open(args.record_keys, "w", buffering=1) if args.record_keys else None

    t_end = time.time() + args.duration
    for t in threads:
        t.start()
    try:
        while time.time() < t_end:
            time.sleep(0.5)
            drain(rows, fh)
            if kh:
                drain(keys, kh)
    except KeyboardInterrupt:
        pass
    finally:
        stop.set()
        for t in threads:
            t.join(timeout=args.timeout + 5)
        drain(rows, fh)
        if kh:
            drain(keys, kh)
            kh.close()
        fh.close()

    return 0


if __name__ == "__main__":
    sys.exit(main())
