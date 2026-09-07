#!/usr/bin/env python3
"""
Puente TLS hacia los KME de ID Quantique (ETSI-014).

Es un PROXY TRANSPARENTE: no traduce ni trocea nada. Existe sólo porque el QKC
no puede abrir el TLS del equipo directamente (certs ECDSA clásicos y cert de
servidor SIN SAN, contra un QKC que es ML-DSA-only y verifica el nombre).
El troceado por `max_key_per_request` y la caída a una petición por clave en
`dec_keys` los hace ya el propio QKC, que los descubre del /status.

Presenta a los QKC del proyecto dkms_rust el mismo dialecto que quditto
(un endpoint compartido con enc_keys?number=N en lote y dec_keys en batch),
y por detras usa los KME reales de ID Quantique del laboratorio UVigo:

  enc_keys / status  ->  KME MAESTRO (Alice)  https://192.168.100.102:443  cert ETSIA
  dec_keys           ->  KME ESCLAVO (Bob)    https://192.168.100.107:443  cert ETSIB

El IDQ limita number=1 y no acepta dec en lote, asi que el shim descompone
cada peticion del QKC en N llamadas de 1 clave y las reagrupa.

Escucha HTTP plano (como un quditto local) en 127.0.0.1:PORT.
"""
import http.server, http.client, json, ssl, sys, threading, time

MASTER = ("192.168.100.102", 443)   # Alice, enc_keys + status
SLAVE  = ("192.168.100.107", 443)   # Bob, dec_keys
SAE_ENC = "ETSIB"                    # el maestro genera "para" el esclavo
SAE_DEC = "ETSIA"                    # el esclavo recupera "del" maestro
CERTS = "/home/pablopio/Documentos/trabajo_atlantic/nodos/certs"

def _ctx(cert, key):
    c = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    c.check_hostname = False              # cert servidor CN=QKDServer != IP
    c.verify_mode = ssl.CERT_NONE
    c.load_cert_chain(certfile=cert, keyfile=key)
    return c

CTX_MASTER = _ctx(f"{CERTS}/alice/ETSIA.pem", f"{CERTS}/alice/ETSIA-key.pem")
CTX_SLAVE  = _ctx(f"{CERTS}/bob/ETSIB.pem",   f"{CERTS}/bob/ETSIB-key.pem")

_stats = {"enc_ok": 0, "enc_fail": 0, "dec_ok": 0, "dec_fail": 0}
_lock = threading.Lock()

def _idq(host, port, ctx, method, path, body=None, timeout=8):
    conn = http.client.HTTPSConnection(host, port, context=ctx, timeout=timeout)
    try:
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"
        conn.request(method, path, body=body, headers=headers)
        r = conn.getresponse()
        data = r.read()
        return r.status, data
    finally:
        conn.close()

def idq_enc_one():
    st, data = _idq(*MASTER, CTX_MASTER, "GET",
                    f"/api/v1/keys/{SAE_ENC}/enc_keys")
    if st != 200:
        return None
    keys = json.loads(data).get("keys", [])
    return keys[0] if keys else None

def idq_dec_one(key_id):
    body = json.dumps({"key_IDs": [{"key_ID": key_id}]})
    st, data = _idq(*SLAVE, CTX_SLAVE, "POST",
                    f"/api/v1/keys/{SAE_DEC}/dec_keys", body=body)
    if st != 200:
        return None
    keys = json.loads(data).get("keys", [])
    return keys[0] if keys else None

def idq_status():
    st, data = _idq(*MASTER, CTX_MASTER, "GET",
                    f"/api/v1/keys/{SAE_ENC}/status")
    if st != 200:
        return None
    return json.loads(data)

class H(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *a):  # silencio; el shim lleva sus propios contadores
        pass

    def _send(self, code, obj):
        payload = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def _raw(self, code, data):
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def _qs(self):
        from urllib.parse import urlparse, parse_qs
        q = parse_qs(urlparse(self.path).query)
        return q

    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/healthz":
            return self._send(200, {"ok": True, "stats": _stats})
        if p.endswith("/status"):
            st = idq_status()
            if st is None:
                return self._send(503, {"message": "IDQ status unavailable"})
            # Passthrough HONESTO: el QKC descubre aquí el límite real del
            # equipo (max_key_per_request=1, key_size=256) y se adapta solo.
            return self._send(200, st)
        if p.endswith("/enc_keys"):
            # Proxy transparente: se pasa `number` tal cual. Si el QKC pide
            # más de lo que el equipo admite, recibe el 400 del equipo — que es
            # justo lo que tiene que aprender del /status y evitar.
            q = self._qs()
            number = q.get("number", ["1"])[0]
            size = q.get("size", ["256"])[0]
            st, data = _idq(*MASTER, CTX_MASTER, "GET",
                            f"/api/v1/keys/{SAE_ENC}/enc_keys?number={number}&size={size}")
            with _lock:
                if st == 200: _stats["enc_ok"] += 1
                else: _stats["enc_fail"] += 1
            return self._raw(st, data)
        return self._send(404, {"message": "not found"})

    def do_POST(self):
        p = self.path.split("?")[0]
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b"{}"
        if p.endswith("/dec_keys"):
            # Proxy transparente: el lote se manda tal cual. Un lote de más de
            # una clave lo rechaza el equipo con 400, y el QKC baja solo a una
            # petición por clave.
            st, data = _idq(*SLAVE, CTX_SLAVE, "POST",
                            f"/api/v1/keys/{SAE_DEC}/dec_keys", body=raw)
            with _lock:
                if st == 200: _stats["dec_ok"] += 1
                else: _stats["dec_fail"] += 1
            return self._raw(st, data)
        return self._send(404, {"message": "not found"})

class Srv(http.server.ThreadingHTTPServer):
    daemon_threads = True

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 20010
    print(f"IDQ->quditto shim en http://127.0.0.1:{port}  (master {MASTER[0]} enc, slave {SLAVE[0]} dec)", flush=True)
    Srv(("127.0.0.1", port), H).serve_forever()
