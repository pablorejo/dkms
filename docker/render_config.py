#!/usr/bin/env python3
"""Render a module's real config (TOML) from a simple `node.yml`, inside the
container entrypoint.

An institution only edits a short `node.yml`; this turns it into exactly what the
Rust binary expects:
  * qkc     -> <out>/qkc.toml        (loaded with `qkc --config`)
  * orr     -> <out>/default.toml    (loaded via CONFIG_DIR=<out>)
  * dkms    -> <out>/default.toml
  * sdn     -> <out>/default.toml    (no topology: it infers it from the
                                      modules' announcements)
  * quditto -> <out>/quditto.env     (sourced by the entrypoint; the binary
                                      takes CLI flags with QUDITTO_* env fallbacks,
                                      so there is no config file to write)

Usage: render_config.py <role> <node.yml> <out_dir>

Stdlib + PyYAML (debian pkg python3-yaml). TOML emitted by hand (configs are
simple). Field names/structure mirror qkc/orr/dkms/sdn `src/config.rs`.
Compatible with Python 3.7+ (no nested same-quote f-strings).
"""
import os
import sys

import yaml

# Fixed default ports per role. One container per module (per machine, host net)
# so there is no cross-module collision; a full "site" on one host also works
# because the per-role ranges are disjoint. Override via node.yml `ports:`.
PORTS = {
    "qkc": {"peer": 20000, "local": 20001, "admin": 20002},
    "orr": {"grpc": 20003, "metrics": 20004},
    "dkms": {"sae": 20005, "peer": 20006, "grpc": 20007, "metrics": 20008, "ack": 20009},
    "sdn": {"grpc": 19000, "http": 19002, "metrics": 19010},
    "quditto": {"http": 20010},
}


def die(msg):
    sys.stderr.write("render_config: ERROR: " + msg + "\n")
    sys.exit(2)


def req(d, key, role):
    if not isinstance(d, dict) or key not in d or d[key] is None:
        die("[" + role + "] node.yml missing required field '" + key + "'")
    return d[key]


def with_port(addr, default_port):
    """Accept 'ip' or 'ip:port'; append default_port if no ':' present."""
    addr = str(addr)
    return addr if ":" in addr else (addr + ":" + str(default_port))


def sdn_http_from(grpc_url):
    """The ORR/DKMS point at the SDN's gRPC; registration lives on its HTTP
    admin. Same host, the HTTP port from PORTS. El ESQUEMA se hereda del
    sdn_url: una SDN con control_tls se escribe `https://...` en el node.yml
    de cada módulo y el anuncio sale mTLS — antes esto emitía http:// SIEMPRE,
    con lo que un despliegue con la SDN en mTLS anunciaba en claro contra un
    puerto TLS para siempre, sin forma de decirlo en node.yml."""
    u = str(grpc_url)
    scheme = "https://" if u.strip().lower().startswith("https://") else "http://"
    rest = u.partition("//")[2] or u
    host = rest.rstrip("/").rsplit(":", 1)[0]
    return scheme + host + ":" + str(PORTS["sdn"]["http"])


def url_with_port(url, default_port):
    """Accept 'ip', 'ip:port', 'http://ip' or 'http://ip:port'."""
    url = str(url)
    scheme, sep, rest = url.partition("//")
    if not sep:
        scheme, rest = "http:", url
        sep = "//"
    host = rest.rstrip("/")
    return scheme + sep + with_port(host, default_port)


def q(s):
    """TOML-quote a string."""
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def write(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def toml_value(v, where):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return str(v)
    if isinstance(v, str):
        return q(v)
    die("[extra] " + where + ": solo escalares (bool/número/string), no " + type(v).__name__)


def merge_extra(lines, extra):
    """Funde el mapa libre `extra:` del node.yml en el TOML renderizado.

    Es la válvula de escape para cualquier campo de `src/config.rs` sin clave
    propia en node.yml (rotation_period_ms ya la tiene; generator.*,
    request.ack_wait_timeout_ms, rate_allocator/num_*, timeouts southbound...)
    sin tener que enumerar cada uno aquí. Reglas, las mismas que muerden a
    mano: un ESCALAR se inserta antes de la primera tabla (una clave suelta
    tras una cabecera pertenece a esa tabla); un dict de UN nivel se funde en
    la tabla si el render ya la emitió (justo tras su cabecera, el patrón sed
    de mesh.sh hecho en serio) o se añade entera al final (seguro incluso tras
    [[links]]: una cabecera nueva cierra el elemento). Colisión con una clave
    que el render ya emitió = error a la vista, nunca un override silencioso.
    """
    for key, val in (extra or {}).items():
        key = str(key)
        if isinstance(val, dict):
            header = "[" + key + "]"
            entries = []
            for k2, v2 in val.items():
                if isinstance(v2, dict):
                    die("[extra] " + key + "." + str(k2) + ": máximo un nivel de anidado")
                entries.append(str(k2) + " = " + toml_value(v2, key + "." + str(k2)))
            if header in lines:
                at = lines.index(header)
                end = next((i for i in range(at + 1, len(lines))
                            if lines[i].startswith("[")), len(lines))
                for e in entries:
                    k2 = e.split(" = ")[0]
                    if any(l.startswith(k2 + " ") for l in lines[at + 1:end]):
                        die("[extra] " + key + "." + k2 + " colisiona con una clave que el "
                            "render ya emite: usa la clave nativa del node.yml")
                lines[at + 1:at + 1] = entries
            else:
                lines += ["", header] + entries
        else:
            if any(l.startswith(key + " ") for l in lines if not l.startswith("[")):
                die("[extra] " + key + " colisiona con una clave que el render ya emite: "
                    "usa la clave nativa del node.yml")
            first_table = next((i for i, l in enumerate(lines) if l.startswith("[")), len(lines))
            lines.insert(first_table, key + " = " + toml_value(val, key))


# ─────────────────────────────── QKC ────────────────────────────────────────
def render_qkc(n, out):
    p = dict(PORTS["qkc"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    key_bits = int(n.get("key_size_bits", 256))
    peer_l = bind + ":" + str(p["peer"])
    local_l = bind + ":" + str(p["local"])
    admin_l = bind + ":" + str(p["admin"])
    lines = [
        "qkc_id = " + str(int(req(n, "qkc_id", "qkc"))),
        "peer_listen = " + q(peer_l),
        "local_listen = " + q(local_l),
        "admin_http = " + q(admin_l),
    ]
    # Identidad de firma ML-DSA de este QKC (seed base64), para enlaces con
    # pqc_auth = sign (docs/SECURITY.md §Fase 5). Solo config local.
    if n.get("sign_secret_seed") is not None:
        lines.append("sign_secret_seed = " + q(str(n["sign_secret_seed"])))
    # Self-registration with the SDN. Optional: without it somebody has to add
    # this node to the SDN's topology by hand.
    if n.get("sdn_url"):
        lines.append("sdn_url = " + q(url_with_port(n["sdn_url"], PORTS["sdn"]["http"])))
        # admin_http binds 0.0.0.0, which is useless as an address for the SDN
        # to dial back, so the announcement needs a routable IP.
        if n.get("advertise_ip"):
            lines.append("advertise_ip = " + q(n["advertise_ip"]))
        if n.get("sdn_announce_secs") is not None:
            lines.append("sdn_announce_secs = " + str(int(n["sdn_announce_secs"])))
    for lk in n.get("links", []):
        nid = int(req(lk, "neighbor_id", "qkc.link"))
        typ = str(lk.get("type", "pqc")).lower()
        lines += ["", "[[links]]", "neighbor_id = " + str(nid),
                  "key_size_bits = " + str(key_bits)]
        # neighbor_addr is optional on a pqc link: the address never travels in
        # the announcement (only the id does), the SDN already knows it because
        # every QKC announces its own, and it comes back in the peer list. So a
        # link may be declared by id alone and the SDN says where it is. A qkd
        # link still needs it -- the SDN does not create those, see below.
        if lk.get("neighbor_addr") is not None:
            lines.append("neighbor_peer_addr = "
                         + q(with_port(lk["neighbor_addr"], PORTS["qkc"]["peer"])))
        elif typ == "qkd":
            die("[qkc.link] a qkd link to " + str(nid) + " needs neighbor_addr: the SDN only "
                "creates pqc links, so nothing would fill it in")
        elif not n.get("sdn_url"):
            die("[qkc.link] link to " + str(nid) + " has no neighbor_addr and this node has no "
                "sdn_url to learn it from: declare one of the two")
        # Physical model of the link. The QKC does not use these — it forwards
        # them to the SDN, which sizes the edge with r0*10^(-alpha*d/10) for
        # qkd links and with capacity_keys_per_s (SDN default 10000) for pqc.
        # Only the institution knows them: its own fibre, or what it configured
        # on the quditto.
        if lk.get("r0") is not None:
            lines.append("r0 = " + str(float(lk["r0"])))
        if lk.get("alpha") is not None:
            lines.append("alpha = " + str(float(lk["alpha"])))
        if lk.get("distance_km") is not None:
            lines.append("distance_km = " + str(int(lk["distance_km"])))
        if lk.get("capacity_keys_per_s") is not None:
            lines.append("capacity_keys_per_s = "
                         + str(float(lk["capacity_keys_per_s"])))
        # Raíz de autenticación del enlace, y política del MAC de los frames de
        # datos. Van FUERA de la bifurcación qkd/pqc a propósito: el OTP no da
        # integridad ni frescura venga el material de un KME o de ML-KEM, y el
        # NOTIFY tampoco lo protege el propio QKD. Estuvieron dentro de la rama
        # pqc y el resultado fue que un enlace qkd con `link_psk` en su node.yml
        # lo perdía al renderizar, sin decir nada (medido 2026-08-28).
        if lk.get("link_psk") is not None:
            lines.append("link_psk = " + q(str(lk["link_psk"])))
        # frame_auth = off|prefer|require: MAC de los frames de DATOS
        # (integridad + autenticación de origen + anti-replay). Es un flag
        # aparte del handshake porque protege otra cosa. SI SE OMITE, el default
        # depende de la identidad de nodo (A3): en un enlace pqc de un nodo con
        # [tls] (control_tls) va `require` por defecto — la raíz del sello es el
        # secreto de la época del enlace, no una PSK, así que no hay nada que
        # repartir. Ponerlo aquí sólo para forzar otro valor (p. ej. `off`).
        if lk.get("frame_auth") is not None:
            lines.append("frame_auth = " + q(str(lk["frame_auth"])))
        if typ == "qkd":
            kme = str(req(lk, "kme_url", "qkc.link(qkd)"))
            if "//" not in kme:
                kme = "https://" + kme
            lines += ['link_type = "qkd"', "quditto_url = " + q(kme)]
            # Credencial hacia ESTE KME (A6): cada KME es una autoridad propia
            # (su PKI privada), así que el QKC lleva 1 cert de red + 1
            # credencial POR KME. O los tres campos o ninguno (lo valida el
            # binario); sin ellos cae al [tls] de red — la simplificación de
            # la prueba con quditto. Rutas verbatim: son montajes del
            # operador, no certs de la net-ca en certs_dir.
            for k in ("kme_cert", "kme_key", "kme_ca"):
                if lk.get(k) is not None:
                    lines.append(k + " = " + q(str(lk[k])))
        else:  # pqc (QKD simulated by PQC)
            lines += ['link_type = "pqc"',
                      "pqc_suite = " + q(lk.get("pqc_suite", "ml-kem-768")),
                      "pqc_rekey_keys = " + str(int(lk.get("pqc_rekey_keys", 1000))),
                      "pqc_rekey_secs = " + str(int(lk.get("pqc_rekey_secs", 3600))),
                      "pqc_rekey_lookahead = " + str(int(lk.get("pqc_rekey_lookahead", 2)))]
            # Autenticación del handshake PQC (docs/SECURITY.md §Fase 5).
            # pqc_auth = off|prefer|require (HMAC con link_psk) | sign (firma
            # ML-DSA). SI SE OMITE, el default depende de la identidad de nodo
            # (A3): con [tls] (control_tls) va `sign` por defecto, firmado con el
            # cert de nodo y verificado contra la net-CA + SAN — sin material
            # por-par, así que hasta los enlaces que crea la SDN quedan firmados.
            # `peer_verify_key` es el camino legacy (clave pública ML-DSA cruda
            # del vecino); el seed propio va a nivel de nodo (sign_secret_seed).
            if lk.get("pqc_auth") is not None:
                lines.append("pqc_auth = " + q(str(lk["pqc_auth"])))
            if lk.get("peer_verify_key") is not None:
                lines.append("peer_verify_key = " + q(str(lk["peer_verify_key"])))
    qkc_cert = n.get("cert_name", "qkc-" + str(int(req(n, "qkc_id", "qkc"))))
    lines += control_tls_lines(n, qkc_cert, "client")
    merge_extra(lines, n.get("extra"))
    write(os.path.join(out, "qkc.toml"), "\n".join(lines) + "\n")


# ─────────────────────────────── ORR ────────────────────────────────────────
def render_orr(n, out):
    p = dict(PORTS["orr"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    default_qkc = "127.0.0.1:" + str(PORTS["qkc"]["local"])
    qkc_addr = with_port(n.get("qkc_addr", default_qkc), PORTS["qkc"]["local"])
    grpc_l = bind + ":" + str(p["grpc"])
    metrics_l = bind + ":" + str(p["metrics"])
    lines = [
        "orr_id = " + q(req(n, "orr_id", "orr")),
        "qkc_id = " + str(int(req(n, "qkc_id", "orr"))),
        "qkc_local_addr = " + q(qkc_addr),   # ORR -> its QKC (localhost or remote IP)
        "grpc_addr = " + q(grpc_l),
        "sdn_url = " + q(n.get("sdn_url", "")),
        "metrics_addr = " + q(metrics_l),
        # 0 = passthrough: el material ya va sellado extremo a extremo por el
        # DKMS (dkms/src/e2e.rs); la cebolla del ORR es privacidad de camino
        # opcional (1, >=2, -1).
        "default_max_hops = " + str(int(n.get("default_max_hops", 0))),
    ]
    # Firma ML-DSA del bootstrap (docs/SECURITY.md §Fase 6 PQC): seed de firma
    # de este ORR (escalar, seguro antes de cualquier tabla). Las verifying
    # keys de los peers van en su [peer_verify_keys] al final (con las tablas).
    if n.get("sign_secret_seed") is not None:
        lines.append("sign_secret_seed = " + q(str(n["sign_secret_seed"])))
    if n.get("bootstrap_trust") is not None:
        lines.append("bootstrap_trust = " + q(str(n["bootstrap_trust"])))
    # Rotación del master_secret ORR↔ORR (forward secrecy), en ms. Escalar:
    # antes de cualquier tabla. Sin clave era inalcanzable desde node.yml y
    # mesh.sh lo parcheaba con sed.
    if n.get("rotation_period_ms") is not None:
        lines.append("rotation_period_ms = " + str(int(n["rotation_period_ms"])))
    # Self-registration. sdn_url is the SDN's gRPC; the registration endpoint
    # lives on its HTTP admin, so derive that port rather than asking for a
    # second URL in node.yml.
    #
    # sdn_http_url goes in whenever sdn_url does, even with no advertise_ip.
    # Gating both on advertise_ip made the ORR bail out on the missing
    # sdn_http_url -- before reaching the branch that explains what is wrong --
    # so the module booted, logged "orr->sdn client connected" and never
    # registered, while its DKMS waited on it forever. Emitting it lets the
    # binary print its own error, like the QKC already does.
    if n.get("sdn_url"):
        lines.append("sdn_http_url = " + q(sdn_http_from(n["sdn_url"])))
        if n.get("advertise_ip"):
            lines.append("advertise_ip = " + q(n["advertise_ip"]))
        if n.get("sdn_announce_secs") is not None:
            lines.append("sdn_announce_secs = " + str(int(n["sdn_announce_secs"])))
    # grpc_tls: mTLS en el gRPC del ORR (DKMS↔ORR y ORR↔ORR). ACTIVADO por
    # defecto: por ese gRPC viaja el material de transporte. Exige la identidad
    # de nodo del ORR —certs/<orr_id>.crt/.key + net-ca.crt—, así que el bloque
    # [tls] sale siempre que grpc_tls esté puesto, haya o no control_tls.
    # `grpc_tls: false` lo deja en claro (sólo DKMS y ORR en la misma máquina o
    # red interna de confianza) y entonces [tls] sólo sale con control_tls.
    # Es un ESCALAR: va antes de CUALQUIER tabla, no solo de [tls]. En TOML una
    # clave suelta tras una tabla pertenece a esa tabla, y ya pasó dos veces:
    # `tls.grpc_tls` (el ORR arrancaba en claro con la opción "puesta") y
    # `peers.grpc_tls` (se emitía después de [peers]: config-rs coercionaba el
    # bool a un peer fantasma `grpc_tls -> 1` y el opt-out `grpc_tls: false`
    # se ignoraba en silencio). Lo fija test_render_config.py.
    grpc_tls = n.get("grpc_tls", True)
    lines.append("grpc_tls = " + ("true" if grpc_tls else "false"))
    # served_dkms (B1): identidades (SAN del cert mTLS) con permiso para la
    # superficie de aplicación del ORR (SendMessage/StreamDeliveries) — su
    # DKMS, no cualquier cert de la net-ca. Si el node.yml no lo declara se
    # deriva del `node_id` del DKMS de ESTA MISMA node.yml (la misma alimenta
    # todos los roles de la institución), así que el despliegue estándar queda
    # acotado sin tocar nada. Sin ninguno de los dos, no se emite y el ORR
    # avisa al primer uso. Escalar (array): antes de cualquier tabla.
    served = n.get("served_dkms")
    if served is None and n.get("node_id") is not None:
        served = [str(n["node_id"])]
    if served:
        lines.append("served_dkms = [" + ", ".join(q(str(s)) for s in served) + "]")
    peers = n.get("peers") or {}           # orr_id -> qkc_id
    if peers:
        lines += ["", "[peers]"] + [str(k) + " = " + str(int(v)) for k, v in peers.items()]
    orr_cert = n.get("cert_name", req(n, "orr_id", "orr"))
    lines += control_tls_lines(n, orr_cert, "client", force=bool(grpc_tls))
    pvk = n.get("peer_verify_keys") or {}  # orr_id -> base64(ML-DSA verify key)
    if pvk:
        lines += ["", "[peer_verify_keys]"] + [q(str(k)) + " = " + q(str(v)) for k, v in pvk.items()]
    pg = n.get("peer_grpc_addrs") or {}    # orr_id -> grpc url
    if pg:
        lines += ["", "[peer_grpc_addrs]"] + [str(k) + " = " + q(v) for k, v in pg.items()]
    merge_extra(lines, n.get("extra"))
    write(os.path.join(out, "default.toml"), "\n".join(lines) + "\n")


# ─────────────────────────────── DKMS ───────────────────────────────────────
def render_dkms(n, out):
    p = dict(PORTS["dkms"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    node_id = req(n, "node_id", "dkms")
    adv = req(n, "advertise_ip", "dkms")   # routable IP of this machine (peers/ACK/cert SAN)
    certs = n.get("certs_dir", "/config/certs")
    default_orr = "127.0.0.1:" + str(PORTS["orr"]["grpc"])
    orr_addr = with_port(n.get("orr_addr", default_orr), PORTS["orr"]["grpc"])
    lines = [
        "node_id = " + q(node_id),
        "default_security_level = " + q(n.get("security_level", "qkd_prefer")),
        # advertise_ip already had to be routable (peers, ACKs, cert SAN), so
        # the announcement reuses it.
        "advertise_ip = " + q(adv),
        "sdn_announce_secs = " + str(int(n.get("sdn_announce_secs", 30))),
        "",
        "[listen]",
        "sae_addr = " + q(bind + ":" + str(p["sae"])),
        "peer_addr = " + q(bind + ":" + str(p["peer"])),
        # Plano de operador (DkmsControl). Sin auth y su RPC Drain borra todo
        # el material de una llamada: localhost, salvo que el node.yml lo
        # abra a propósito con `control_addr: <ip>[:puerto]`.
        "grpc_addr = " + q(with_port(n.get("control_addr", "127.0.0.1"), p["grpc"])),
        "metrics_addr = " + q(bind + ":" + str(p["metrics"])),
        "",
        "[tls]",
        "cert_path = " + q(certs + "/" + node_id + ".crt"),
        "key_path = " + q(certs + "/" + node_id + ".key"),
        # Dos raíces separadas (docs/SECURITY.md §2): SAEs verificados por
        # sae-ca; peers DKMS y control plane por net-ca. Antes ambas eran
        # ca.crt, lo que hacía que un cert de SAE valiera como cert de DKMS.
        "sae_client_ca = " + q(certs + "/sae-ca.crt"),
        "peer_dkms_ca = " + q(certs + "/net-ca.crt"),
        "control_plane_ca = " + q(certs + "/net-ca.crt"),
        "",
        "[southbound]",
        "sdn_endpoint = " + q(n.get("sdn_endpoint", "")),
        # orr_tls (default true): el ORR corre con grpc_tls y se le habla por
        # https con el cert de nodo de este DKMS. `orr_tls: false` va en claro,
        # y hay que escribirlo también en el TOML: el binario sube http→https
        # por su cuenta salvo que se le diga que no.
        "orr_tls = " + ("true" if n.get("orr_tls", True) else "false"),
        "orr_endpoint = " + q(("https://" if n.get("orr_tls", True) else "http://") + orr_addr),
        # Self-registration: the SDN places this DKMS under its ORR, so it
        # needs the ORR's *id*, not just its address. sdn_endpoint is gRPC;
        # the registration endpoint is on the SDN's HTTP admin.
        "orr_id = " + q(n.get("orr_id", "orr_" + str(node_id).split("-")[-1])),
        "sdn_http_url = " + q(sdn_http_from(n.get("sdn_endpoint", ""))),
        "",
        "[generator]",
        # bindea al mismo listen_ip que el resto (0.0.0.0 por defecto = una
        # máquina; una IP concreta si varios DKMS comparten host, p.ej. tests).
        "ack_socket_addr = " + q(bind + ":" + str(p["ack"])),
        "ack_advertised_endpoint = " + q(adv + ":" + str(p["ack"])),
        # Transporte de los ACK salientes: `socket` (TCP plano, heredado) o
        # `etsi020` (POST mTLS, identidad = cert). Y si se ESCUCHA el socket:
        # en un despliegue mixto hay que seguir escuchando a los peers que
        # aún acusan por él; con todos en etsi020 se apaga.
        "ack_transport = " + q(str(n.get("ack_transport", "socket"))),
        "ack_socket_listen = " + ("true" if n.get("ack_socket_listen", True) else "false"),
    ]
    fr = n.get("fill_rate")
    if fr is not None:
        lines.append("default_fill_rate_keys_per_s = " + str(float(fr)))
    # Tuning caliente con clave propia: el tamaño del buffer por peer (B_k
    # del solver, el knob que las campañas suben de 4096) y la capa e2e
    # DKMS<->DKMS (rekey_secs para acelerar la rotación en pruebas). El resto
    # de campos sin clave van por `extra:`.
    if n.get("capacity_per_peer") is not None:
        lines += ["", "[buffer]",
                  "capacity_per_peer = " + str(int(n["capacity_per_peer"]))]
    te = n.get("transport_e2e") or {}
    if te:
        lines += ["", "[transport_e2e]"]
        for k in ("suite", "rekey_secs", "rekey_keys", "epoch_history_keep", "replay_window"):
            if te.get(k) is not None:
                v = te[k]
                lines.append(k + " = " + (q(str(v)) if k == "suite" else str(int(v))))
    # Autorización de SAEs: fail-closed contra `sae_bindings` (los SAE que este
    # nodo declara servir). Con la lista vacía el DKMS no sirve a nadie, así
    # que se avisa aquí, en el render, antes del primer 404.
    authz = bool(n.get("sae_authorization", True))
    lines += ["", "[sae]", "enforce_authorization = " + ("true" if authz else "false")]
    binds = n.get("sae_bindings") or {}
    if authz and not any(str(v) == str(node_id) for v in binds.values()):
        sys.stderr.write(
            "render_config: AVISO dkms " + str(node_id) + ": sae_authorization está activo y "
            "ningún sae_bindings apunta a este nodo — toda petición SAE recibirá 404. "
            "Añade `sae_bindings: {<sae_id>: " + str(node_id) + "}` o `sae_authorization: false`.\n")
    if binds:
        lines += ["", "[sae_bindings]"] + [q(k) + " = " + q(v) for k, v in binds.items()]
    for pid, pc in (n.get("peers") or {}).items():
        ep = with_port(req(pc, "endpoint", "dkms.peers." + str(pid)), PORTS["dkms"]["peer"])
        if "//" not in ep:
            ep = "https://" + ep
        orr_id = pc.get("orr_id", "orr_" + str(pid).split("-")[-1])
        lines += ["", "[peers." + str(pid) + "]", "endpoint = " + q(ep),
                  'transport = "orr"', "orr_id = " + q(orr_id)]
        # Override por-peer del modo de routing ORR. Sin esto se usa
        # default_max_hops (0 = passthrough de fábrica). Subirlo a >=2 (cebolla
        # multi-salto) requiere además orr_path (CSV de orr_ids) mientras la SDN
        # no calcule paths; con 1 (cebolla E2E) no hace falta.
        if pc.get("max_hops") is not None:
            lines.append("max_hops = " + str(int(pc["max_hops"])))
        if pc.get("orr_path") is not None:
            lines.append("orr_path = " + q(str(pc["orr_path"])))
    merge_extra(lines, n.get("extra"))
    write(os.path.join(out, "default.toml"), "\n".join(lines) + "\n")


# ─────────────────────────────── SDN ────────────────────────────────────────
def control_tls_lines(n, node_id, kind, force=False):
    """[tls] block for control-plane mTLS (docs/SECURITY.md §Fase 3).
    Emitted when node.yml sets `control_tls: true`, or when the caller needs
    the identity anyway (`force`: the ORR's gRPC runs mTLS by default). `kind`
    selects the field shape: the SDN is a server (cert/key/client_ca), orr/qkc
    are clients (cert/key/control_plane_ca)."""
    if not (n.get("control_tls") or force):
        return []
    certs = n.get("certs_dir", "/config/certs")
    lines = ["", "[tls]",
             "cert_path = " + q(certs + "/" + node_id + ".crt"),
             "key_path = " + q(certs + "/" + node_id + ".key")]
    if kind == "server":
        lines.append("client_ca = " + q(certs + "/net-ca.crt"))
    else:
        lines.append("control_plane_ca = " + q(certs + "/net-ca.crt"))
    return lines


def render_sdn(n, out):
    """The SDN no longer takes a topology: it builds one from what the modules
    announce (POST /register/{qkc,orr,dkms}). So its node.yml is just its own
    listen addresses and timers."""
    p = dict(PORTS["sdn"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    lines = [
        "node_id = \"sdn\"",
        "grpc_addr = " + q(bind + ":" + str(p["grpc"])),
        "http_addr = " + q(bind + ":" + str(p["http"])),
        "metrics_addr = " + q(bind + ":" + str(p["metrics"])),
        # How long a module may go quiet before being dropped. Must exceed
        # the modules' sdn_announce_secs; 0 disables expiry.
        "presence_ttl_secs = " + str(int(n.get("presence_ttl_secs", 90))),
    ]
    # Solo si el operador los pone: los defaults canónicos viven en el binario
    # (mcf_period_ms 5000, push_debounce_ms 100) — el render los pisaba y
    # divergían tres fuentes (código 1000 / render 5000 / README 5000).
    if n.get("mcf_period_ms") is not None:
        lines.append("mcf_period_ms = " + str(int(n["mcf_period_ms"])))
    if n.get("push_debounce_ms") is not None:
        lines.append("push_debounce_ms = " + str(int(n["push_debounce_ms"])))
    if n.get("http_ro_port"):
        lines.append("http_ro_addr = " + q(bind + ":" + str(int(n["http_ro_port"]))))
    # cert_name como en qkc/orr: por defecto "sdn" (gen-certs.sh sdn <ip>).
    lines += control_tls_lines(n, n.get("cert_name", "sdn"), "server")
    merge_extra(lines, n.get("extra"))
    write(os.path.join(out, "default.toml"), "\n".join(lines) + "\n")


# ───────────────────────────── quditto ──────────────────────────────────────
def render_quditto(n, out):
    """Simulated QKD link. Serves ETSI-014 to the two QKCs of one link, minting
    random keys at R(d) = r0 * 10^(-alpha*d/10).

    Unlike the other roles the binary reads CLI flags (with QUDITTO_* env
    fallbacks) rather than a config file, so we emit a shell env file that the
    entrypoint sources.
    """
    p = dict(PORTS["quditto"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    env = [
        ("QUDITTO_LISTEN", bind + ":" + str(p["http"])),
        ("QUDITTO_R0", str(float(req(n, "r0", "quditto")))),
        ("QUDITTO_ALPHA", str(float(n.get("alpha", 0.2)))),
        ("QUDITTO_DISTANCE", str(float(n.get("distance_km", 0)))),
        ("QUDITTO_MAX_BUFFER", str(int(n.get("max_buffer", 8192)))),
        ("QUDITTO_KEY_SIZE_BITS", str(int(n.get("key_size_bits", 256)))),
    ]
    # mTLS por defecto: por este ETSI-014 viajan los pads OTP. `tls: false`
    # (claro) SOLO si el QKC consumidor corre en el mismo host — y entonces su
    # quditto_url debe ser http://. Certs: gen-certs.sh <cert_name> <ip>.
    if n.get("tls", True):
        certs = n.get("certs_dir", "/config/certs")
        cert = str(n.get("cert_name", "quditto"))
        env += [
            ("QUDITTO_TLS", "on"),
            ("QUDITTO_TLS_CERT", certs + "/" + cert + ".crt"),
            ("QUDITTO_TLS_KEY", certs + "/" + cert + ".key"),
            ("QUDITTO_TLS_CLIENT_CA", certs + "/net-ca.crt"),
        ]
    else:
        env += [("QUDITTO_TLS", "off")]
    write(os.path.join(out, "quditto.env"),
          "".join("export " + k + "=" + v + "\n" for k, v in env))


ROLES = {"qkc": render_qkc, "orr": render_orr, "dkms": render_dkms, "sdn": render_sdn,
         "quditto": render_quditto}


def main():
    if len(sys.argv) != 4 or sys.argv[1] not in ROLES:
        die("usage: render_config.py <" + "|".join(ROLES) + "> <node.yml> <out_dir>")
    role, node_yml, out = sys.argv[1], sys.argv[2], sys.argv[3]
    with open(node_yml) as f:
        n = yaml.safe_load(f) or {}
    os.makedirs(out, exist_ok=True)
    ROLES[role](n, out)
    print("render_config: wrote " + role + " config to " + out)


if __name__ == "__main__":
    main()
