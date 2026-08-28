#!/usr/bin/env python3
"""Proxy TCP que se sienta entre dos QKC y **modifica** frames al vuelo.

Es el atacante del cable: no conoce ninguna clave, sólo puede tocar bytes que
pasan. Sirve para comprobar de verdad la propiedad que el MAC de frame promete
—"que nadie lo modifique sin que se note"— en un sistema en marcha, y no sólo
en un test unitario.

Sin MAC, esto es indetectable: el payload va cifrado con OTP, que es maleable,
así que un bit volteado en el ciphertext sale como un bit volteado en el
plaintext y el receptor se lo traga. Con `frame_auth = require`, cada frame
tocado tiene que aparecer como `bad_mac` en el peer y NO llegar al DKMS.

    frame_tamper.py --listen 29100 --to 127.0.0.1:20100 --every 50

Parsea el wire de verdad (`wire/src/lib.rs`) en vez de voltear bits a ciegas:
así el frame sigue siendo sintácticamente válido y lo que falla es el MAC, no
el parser. Si se corrompiera el prefijo, el peer lo tiraría por `BadMagic` o
`Truncated` y la prueba no diría nada sobre integridad.

Wire:  MAGIC(4) KIND(1) GRADE(1) LEN(4 LE) BODY(LEN)
"""
import argparse
import socket
import sys
import threading

MAGIC = b"\x51\x4b\x43\x03"
PREFIX = 10


def recv_exactly(sock, n):
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            return None
        buf += chunk
    return buf


def pump(src, dst, every, stats, tamper):
    """Reenvía frame a frame. Cada `every` frames, voltea un bit del body."""
    n = 0
    try:
        while True:
            prefix = recv_exactly(src, PREFIX)
            if prefix is None:
                return
            if prefix[:4] != MAGIC:
                # No es nuestro wire: reenvía tal cual y deja de parsear.
                dst.sendall(prefix)
                while True:
                    chunk = src.recv(65536)
                    if not chunk:
                        return
                    dst.sendall(chunk)
            body_len = int.from_bytes(prefix[6:10], "little")
            body = recv_exactly(src, body_len)
            if body is None:
                return
            n += 1
            if tamper and every > 0 and n % every == 0 and body_len > 0:
                # El ÚLTIMO byte del body: cae dentro del payload (o de su
                # trailer de autenticación), nunca en las longitudes.
                b = bytearray(body)
                b[-1] ^= 0x01
                body = bytes(b)
                stats["tampered"] += 1
            dst.sendall(prefix + body)
            stats["frames"] += 1
    except OSError:
        return


def handle(client, target, every, stats):
    try:
        upstream = socket.create_connection(target)
    except OSError as e:
        print("proxy: no puedo conectar a %s: %s" % (target, e), file=sys.stderr)
        client.close()
        return
    client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    upstream.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    # Sólo se toca el sentido cliente→servidor: es el que lleva los frames de
    # datos hacia el peer que queremos ver rechazarlos.
    threading.Thread(
        target=pump, args=(client, upstream, every, stats, True), daemon=True
    ).start()
    pump(upstream, client, 0, stats, False)
    client.close()
    upstream.close()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--listen", type=int, required=True)
    ap.add_argument("--to", required=True, help="host:puerto del QKC real")
    ap.add_argument("--every", type=int, default=50,
                    help="voltea un bit cada N frames (0 = sólo reenviar)")
    a = ap.parse_args()
    host, port = a.to.rsplit(":", 1)
    target = (host, int(port))

    stats = {"frames": 0, "tampered": 0}
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", a.listen))
    srv.listen(64)
    print("proxy: :%d -> %s, un bit cada %d frames" % (a.listen, a.to, a.every),
          flush=True)

    def report():
        import time
        while True:
            time.sleep(5)
            print("proxy: frames=%d tocados=%d" % (stats["frames"], stats["tampered"]),
                  flush=True)

    threading.Thread(target=report, daemon=True).start()
    while True:
        c, _ = srv.accept()
        threading.Thread(target=handle, args=(c, target, a.every, stats),
                         daemon=True).start()


if __name__ == "__main__":
    main()
