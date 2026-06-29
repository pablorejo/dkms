#!/usr/bin/env python3
"""E2E test of QKD/PQC security levels against a running deployment.

Topology (line N=4): edge 1-2 = PQC, 2-3 = QKD, 3-4 = QKD.
  => node 1 is QKD-isolated (only PQC edge); pair 2-4 is multi-hop QKD (2-3-4).

Sends ETSI 014 enc_keys with a `security_level` extension and asserts the
expected HTTP status per the design:
  * strict_qkd to a QKD-unreachable peer  -> 4xx (admission reject)
  * strict_qkd over a multi-hop QKD path   -> 200 (QKD-grade served)
  * qkd_prefer to a PQC-only peer          -> 200 (PQC fallback)
  * no_worry                               -> 200
Stdlib only.
"""
import http.client
import json
import ssl
import sys
from urllib.parse import urlparse

PLAN = sys.argv[1]
plan = json.load(open(PLAN))
byid = {a["sae_id"]: a for a in plan["sae_assign"]}


def ctx(a):
    c = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    c.load_cert_chain(certfile=a["cert"], keyfile=a["key"])
    return c


def enc(master, slave, level, mandatory):
    a = byid[master]
    extkey = "extension_mandatory" if mandatory else "extension_optional"
    body = json.dumps(
        {"number": 1, "size": 256, extkey: [{"security_level": level}]}
    )
    u = urlparse(a["url"].rstrip("/") + f"/api/v1/keys/{slave}/enc_keys")
    conn = http.client.HTTPSConnection(u.hostname, u.port, context=ctx(a), timeout=10)
    try:
        conn.request("POST", u.path, body=body,
                     headers={"Content-Type": "application/json"})
        r = conn.getresponse()
        return r.status, r.read(300)
    finally:
        conn.close()


# (master, slave, level, mandatory, expected_predicate, label)
def is4xx(s):
    return 400 <= s < 500
def is200(s):
    return s == 200


cases = [
    ("sae_1_0", "sae_2_0", "strict_qkd", True,  is4xx, "strict_qkd node1->node2 (QKD-isolated) => 4xx"),
    ("sae_2_0", "sae_4_0", "strict_qkd", True,  is200, "strict_qkd node2->node4 (multi-hop QKD) => 200"),
    ("sae_3_0", "sae_4_0", "strict_qkd", True,  is200, "strict_qkd node3->node4 (direct QKD)    => 200"),
    ("sae_1_0", "sae_2_0", "qkd_prefer", False, is200, "qkd_prefer node1->node2 (PQC fallback)  => 200"),
    ("sae_2_0", "sae_4_0", "no_worry",   False, is200, "no_worry   node2->node4                 => 200"),
]

print("=== security-level e2e ===")
passed = 0
for master, slave, level, mand, pred, label in cases:
    try:
        st, data = enc(master, slave, level, mand)
        ok = pred(st)
    except Exception as e:  # noqa: BLE001
        st, data, ok = f"EXC:{type(e).__name__}", str(e).encode(), False
    passed += ok
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}  (got {st})"
          + ("" if ok else f"  body={data[:160]!r}"))

print(f"=== {passed}/{len(cases)} passed ===")
sys.exit(0 if passed == len(cases) else 1)
