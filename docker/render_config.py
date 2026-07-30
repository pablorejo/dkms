#!/usr/bin/env python3
"""Render a module's real config (TOML, + SDN topology JSON) from a simple
`node.yml`, inside the container entrypoint.

An institution only edits a short `node.yml`; this turns it into exactly what the
Rust binary expects:
  * qkc     -> <out>/qkc.toml        (loaded with `qkc --config`)
  * orr     -> <out>/default.toml    (loaded via CONFIG_DIR=<out>)
  * dkms    -> <out>/default.toml
  * sdn     -> <out>/default.toml + <out>/topology/{QKC,ORR,DKMS,SAE}/*.json
  * quditto -> <out>/quditto.env     (sourced by the entrypoint; the binary
                                      takes CLI flags with QUDITTO_* env fallbacks,
                                      so there is no config file to write)

Usage: render_config.py <role> <node.yml> <out_dir>

Stdlib + PyYAML (debian pkg python3-yaml). TOML emitted by hand (configs are
simple). Field names/structure mirror qkc/orr/dkms/sdn `src/config.rs` and the
topology JSON the SDN loads.
Compatible with Python 3.7+ (no nested same-quote f-strings).
"""
import json
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


def q(s):
    """TOML-quote a string."""
    return '"' + str(s).replace("\\", "\\\\").replace('"', '\\"') + '"'


def write(path, text):
    os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
    with open(path, "w") as f:
        f.write(text)


def jwrite(path, obj):
    write(path, json.dumps(obj, indent=2))


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
    for lk in n.get("links", []):
        nid = int(req(lk, "neighbor_id", "qkc.link"))
        naddr = with_port(req(lk, "neighbor_addr", "qkc.link"), PORTS["qkc"]["peer"])
        typ = str(lk.get("type", "pqc")).lower()
        lines += ["", "[[links]]", "neighbor_id = " + str(nid),
                  "neighbor_peer_addr = " + q(naddr), "key_size_bits = " + str(key_bits)]
        if typ == "qkd":
            kme = str(req(lk, "kme_url", "qkc.link(qkd)"))
            if "//" not in kme:
                kme = "https://" + kme
            lines += ['link_type = "qkd"', "quditto_url = " + q(kme)]
        else:  # pqc (QKD simulated by PQC)
            lines += ['link_type = "pqc"',
                      "pqc_suite = " + q(lk.get("pqc_suite", "ml-kem-768")),
                      "pqc_rekey_keys = " + str(int(lk.get("pqc_rekey_keys", 1000))),
                      "pqc_rekey_secs = " + str(int(lk.get("pqc_rekey_secs", 3600))),
                      "pqc_rekey_lookahead = " + str(int(lk.get("pqc_rekey_lookahead", 2)))]
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
        "default_max_hops = " + str(int(n.get("default_max_hops", 1))),
    ]
    peers = n.get("peers") or {}           # orr_id -> qkc_id
    if peers:
        lines += ["", "[peers]"] + [str(k) + " = " + str(int(v)) for k, v in peers.items()]
    pg = n.get("peer_grpc_addrs") or {}    # orr_id -> grpc url
    if pg:
        lines += ["", "[peer_grpc_addrs]"] + [str(k) + " = " + q(v) for k, v in pg.items()]
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
        "",
        "[listen]",
        "sae_addr = " + q(bind + ":" + str(p["sae"])),
        "peer_addr = " + q(bind + ":" + str(p["peer"])),
        "grpc_addr = " + q(bind + ":" + str(p["grpc"])),
        "metrics_addr = " + q(bind + ":" + str(p["metrics"])),
        "",
        "[tls]",
        "cert_path = " + q(certs + "/" + node_id + ".crt"),
        "key_path = " + q(certs + "/" + node_id + ".key"),
        "sae_client_ca = " + q(certs + "/ca.crt"),
        "peer_dkms_ca = " + q(certs + "/ca.crt"),
        "",
        "[southbound]",
        "sdn_endpoint = " + q(n.get("sdn_endpoint", "")),
        "qkc_endpoint = " + q("http://127.0.0.1:1"),   # dead by design (transport=orr)
        "orr_endpoint = " + q("http://" + orr_addr),
        "",
        "[generator]",
        # bindea al mismo listen_ip que el resto (0.0.0.0 por defecto = una
        # máquina; una IP concreta si varios DKMS comparten host, p.ej. tests).
        "ack_socket_addr = " + q(bind + ":" + str(p["ack"])),
        "ack_advertised_endpoint = " + q(adv + ":" + str(p["ack"])),
    ]
    fr = n.get("fill_rate")
    if fr is not None:
        lines.append("default_fill_rate_keys_per_s = " + str(float(fr)))
    binds = n.get("sae_bindings") or {}
    if binds:
        lines += ["", "[sae_bindings]"] + [q(k) + " = " + q(v) for k, v in binds.items()]
    for pid, pc in (n.get("peers") or {}).items():
        ep = with_port(req(pc, "endpoint", "dkms.peers." + str(pid)), PORTS["dkms"]["peer"])
        if "//" not in ep:
            ep = "https://" + ep
        orr_id = pc.get("orr_id", "orr_" + str(pid).split("-")[-1])
        lines += ["", "[peers." + str(pid) + "]", "endpoint = " + q(ep),
                  'transport = "orr"', "orr_id = " + q(orr_id)]
    write(os.path.join(out, "default.toml"), "\n".join(lines) + "\n")


# ─────────────────────────────── SDN ────────────────────────────────────────
def render_sdn(n, out):
    p = dict(PORTS["sdn"]); p.update(n.get("ports") or {})
    bind = n.get("listen_ip", "0.0.0.0")
    topo = os.path.join(out, "topology")
    for sub in ("QKC", "ORR", "DKMS", "SAE"):
        os.makedirs(os.path.join(topo, sub), exist_ok=True)

    nodes = req(n, "nodes", "sdn")          # id -> {qkc, orr, dkms} host[:port]
    adj = {}
    for lk in n.get("links", []):
        a, b = int(req(lk, "a", "sdn.link")), int(req(lk, "b", "sdn.link"))
        ch = {
            "distance": lk.get("distance_km", 5),
            "quditto_rate_r0": lk.get("r0", 2000),
            "quditto_rate_alpha": lk.get("alpha", 0.2),
            "quditto_max_buffer_size": lk.get("max_buffer", 65536),
            "link_type": str(lk.get("type", "pqc")).lower(),
        }
        adj.setdefault(a, []).append((b, ch))
        adj.setdefault(b, []).append((a, ch))

    for sid, addrs in nodes.items():
        nid = int(sid)
        qh = with_port(req(addrs, "qkc", "sdn.nodes"), PORTS["qkc"]["admin"])
        oh = with_port(req(addrs, "orr", "sdn.nodes"), PORTS["orr"]["grpc"])
        dh = with_port(req(addrs, "dkms", "sdn.nodes"), PORTS["dkms"]["sae"])
        qip, qport = qh.rsplit(":", 1)
        oip, oport = oh.rsplit(":", 1)
        dip, dport = dh.rsplit(":", 1)
        kmes = [{"neighbor_qkc_id": str(j), "channel": ch} for (j, ch) in adj.get(nid, [])]
        jwrite(os.path.join(topo, "QKC", "qkc-" + str(nid) + ".json"),
               {"id": str(nid), "host": {"id": nid, "ip": qip, "port": int(qport)}, "kmes": kmes})
        jwrite(os.path.join(topo, "ORR", "orr_" + str(nid) + ".json"),
               {"id": "orr_" + str(nid), "qkc_id": str(nid),
                "host": {"id": 200 + nid, "ip": oip, "port": int(oport)}})
        jwrite(os.path.join(topo, "DKMS", "dkms-" + str(nid) + ".json"),
               {"id": "dkms-" + str(nid), "orr_id": "orr_" + str(nid),
                "host": {"id": 300 + nid, "ip": dip, "port": int(dport)}})
    for s in (n.get("saes") or []):
        sid = req(s, "id", "sdn.saes")
        jwrite(os.path.join(topo, "SAE", str(sid) + ".json"),
               {"id": sid, "dkms_id": "dkms-" + str(int(req(s, "node", "sdn.saes")))})

    write(os.path.join(out, "default.toml"),
          "node_id = \"sdn\"\n"
          "grpc_addr = " + q(bind + ":" + str(p["grpc"])) + "\n"
          "http_addr = " + q(bind + ":" + str(p["http"])) + "\n"
          "metrics_addr = " + q(bind + ":" + str(p["metrics"])) + "\n"
          "topology_dir = " + q(topo) + "\n"
          "default_policy = \"shortest_hops\"\n"
          "mcf_period_ms = " + str(int(n.get("mcf_period_ms", 5000))) + "\n"
          "push_debounce_ms = 100\n")


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
