#!/usr/bin/env python3
"""Simulador SAE para tests de carga.

Mantiene una conexión HTTPS al DKMS configurado (mTLS) y loopea
peticiones `POST /api/v1/keys/{slave_sae}/enc_keys` lo más rápido
posible. Para cada respuesta loguea:

    timestamp_unix_s status

donde `status` es 200 (ok) o 429 (rate-limited) o el código HTTP
recibido (errores TLS / red salen como `ERR`).

Uso:
    sae_sim.py --sae-id sae_001 --slave sae_050 \
               --url https://127.0.0.1:8411 \
               --cert tls/sae_001.crt --key tls/sae_001.key \
               --ca   tls/ca.crt \
               --log  /tmp/sae_001.log \
               --duration 200
"""
from __future__ import annotations

import argparse
import json
import os
import ssl
import sys
import time
import urllib.request
from http.client import HTTPSConnection


def make_https_connection(host: str, port: int, certfile: str, keyfile: str, cafile: str) -> HTTPSConnection:
    # Nota: Python 3.10+ exige que el CA tenga la extensión keyUsage, lo cual
    # nuestro CA self-signed de la demo no incluye → SSLCertVerificationError.
    # Curl no aplica esa check estricta. Como esto es un simulador de carga
    # local (host=127.0.0.1) y la confianza del servidor no es la propiedad
    # que estamos midiendo, desactivamos la verificación del cert del server.
    # El client cert (mTLS) sigue cargándose y mandándose al server, que sí
    # lo valida contra `tls.sae_client_ca`.
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    ctx.load_cert_chain(certfile=certfile, keyfile=keyfile)
    # HTTP/1.1 keep-alive: la conexión se reusa entre requests.
    return HTTPSConnection(host, port, context=ctx, timeout=5.0)


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--sae-id", required=True)
    p.add_argument("--slave", required=True)
    p.add_argument("--url", required=True, help="https://host:port")
    p.add_argument("--cert", required=True)
    p.add_argument("--key", required=True)
    p.add_argument("--ca", required=True)
    p.add_argument("--log", required=True)
    p.add_argument("--duration", type=float, default=60.0,
                   help="segundos de loop antes de salir")
    p.add_argument("--size-bits", type=int, default=256)
    p.add_argument("--number", type=int, default=1)
    p.add_argument("--rate-cap", type=float, default=0.0,
                   help="cap superior keys/s (0=ilimitado, dispara tantas como pueda)")
    args = p.parse_args()

    if not args.url.startswith("https://"):
        print("--url debe empezar por https://", file=sys.stderr)
        return 2
    host_port = args.url[len("https://"):]
    host, _, port_s = host_port.partition(":")
    port = int(port_s or "443")

    body = json.dumps({"number": args.number, "size": args.size_bits}).encode()
    path = f"/api/v1/keys/{args.slave}/enc_keys"

    deadline = time.time() + args.duration
    min_interval = (1.0 / args.rate_cap) if args.rate_cap > 0 else 0.0

    # Open log; line-buffered.
    os.makedirs(os.path.dirname(args.log) or ".", exist_ok=True)
    log_f = open(args.log, "a", buffering=1)

    conn = None
    last_send = 0.0
    n_ok = n_429 = n_err = 0
    while time.time() < deadline:
        if min_interval > 0:
            now = time.time()
            sleep_for = min_interval - (now - last_send)
            if sleep_for > 0:
                time.sleep(sleep_for)
        last_send = time.time()
        try:
            if conn is None:
                conn = make_https_connection(host, port, args.cert, args.key, args.ca)
            conn.request("POST", path, body=body, headers={"content-type": "application/json"})
            resp = conn.getresponse()
            # consume body so connection can be reused
            _ = resp.read()
            code = resp.status
            ts = time.time()
            log_f.write(f"{ts:.6f} {code}\n")
            if code == 200:
                n_ok += 1
            elif code == 429:
                n_429 += 1
            else:
                n_err += 1
        except Exception as e:
            n_err += 1
            log_f.write(f"{time.time():.6f} ERR\n")
            # Reset connection on error
            try:
                if conn is not None:
                    conn.close()
            except Exception:
                pass
            conn = None
            time.sleep(0.05)  # breve backoff

    log_f.write(f"# summary sae={args.sae_id} ok={n_ok} rate_limited={n_429} err={n_err}\n")
    log_f.close()
    try:
        if conn is not None:
            conn.close()
    except Exception:
        pass
    return 0


if __name__ == "__main__":
    sys.exit(main())
