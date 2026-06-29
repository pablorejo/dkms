#!/usr/bin/env python3
"""Generic QKD/PQC security-level e2e against a running STATIC-SAE deployment.

Classifies every DKMS pair by QKD-subgraph connectivity (parsed from the
generated QKC TOMLs), then for a sample of QKD-connected and QKD-disconnected
pairs verifies, via real ETSI-014 mTLS requests:

  * default (qkd_prefer): enc -> dec -> byte-match  (must match)
  * no_worry:             enc -> dec -> byte-match  (must match)
  * strict_qkd:           enc 200 + match if the pair is QKD-connected,
                          else 4xx (admission reject)

Exit 0 iff every assertion holds. Stdlib only. Usage: seclevels_e2e.py <run_dir>
"""
import glob
import http.client
import json
import os
import re
import ssl
import sys
from urllib.parse import urlparse

RUN = sys.argv[1]
plan = json.load(open(os.path.join(RUN, "plan.json")))
sae = {a["sae_id"]: a for a in plan.get("sae_assign", [])}
if not sae:
    print("FATAL: no sae_assign (need a static-SAE deploy, i.e. --pairs 0)")
    sys.exit(2)


def node_of(sae_id):
    m = re.match(r"sae_(\d+)_", sae_id)
    return m.group(1) if m else None


# --- QKD-subgraph components from the QKC TOMLs ---------------------------
qkd_adj = {}
for f in glob.glob(os.path.join(RUN, "sites", "site-*", "qkc.toml")):
    me = re.search(r"site-(\d+)", f).group(1)
    qkd_adj.setdefault(me, set())
    cur, is_pqc = None, False

    def flush():
        if cur is not None and not is_pqc:
            qkd_adj.setdefault(me, set()).add(cur)
            qkd_adj.setdefault(cur, set()).add(me)

    for ln in open(f):
        m = re.match(r"\s*neighbor_id\s*=\s*(\d+)", ln)
        if m:
            flush()
            cur, is_pqc = m.group(1), False
        elif 'link_type = "pqc"' in ln:
            is_pqc = True
    flush()

nodes = sorted(qkd_adj, key=int)
comp = {}
for n in nodes:
    if n in comp:
        continue
    stack, cid = [n], n
    comp[n] = cid
    while stack:
        x = stack.pop()
        for y in qkd_adj.get(x, ()):
            if y not in comp:
                comp[y] = cid
                stack.append(y)


def qkd_connected(a, b):
    return a == b or comp.get(a) == comp.get(b)


# --- HTTP helpers --------------------------------------------------------
def ctx(a):
    c = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    c.load_cert_chain(certfile=a["cert"], keyfile=a["key"])
    return c


def post(url, c, body, timeout=10):
    u = urlparse(url)
    conn = http.client.HTTPSConnection(u.hostname, u.port, context=c, timeout=timeout)
    try:
        conn.request("POST", u.path, body=body, headers={"Content-Type": "application/json"})
        r = conn.getresponse()
        return r.status, r.read(2000)
    finally:
        conn.close()


def enc(m_sae, s_sae, level=None, mandatory=False):
    a = sae[m_sae]
    req = {"number": 1, "size": 256}
    if level:
        req["extension_mandatory" if mandatory else "extension_optional"] = [
            {"security_level": level}
        ]
    url = a["url"].rstrip("/") + f"/api/v1/keys/{s_sae}/enc_keys"
    return post(url, ctx(a), json.dumps(req))


def dec(m_sae, s_sae, key_id):
    a = sae[s_sae]
    url = a["url"].rstrip("/") + f"/api/v1/keys/{m_sae}/dec_keys"
    return post(url, ctx(a), json.dumps({"key_IDs": [{"key_ID": key_id}]}))


def roundtrip(m_sae, s_sae, level=None, mandatory=False):
    """Returns (ok, detail). ok=True iff enc 200 + dec 200 + byte-match."""
    st, data = enc(m_sae, s_sae, level, mandatory)
    if st != 200:
        return False, f"enc {st}: {data[:120]!r}"
    try:
        k = json.loads(data)["keys"][0]
        kid, km = k["key_ID"], k["key"]
    except Exception as e:  # noqa: BLE001
        return False, f"enc parse: {e}"
    st2, data2 = dec(m_sae, s_sae, kid)
    if st2 != 200:
        return False, f"dec {st2}: {data2[:120]!r}"
    try:
        ks = json.loads(data2)["keys"][0]["key"]
    except Exception as e:  # noqa: BLE001
        return False, f"dec parse: {e}"
    return (km == ks), ("match" if km == ks else "BYTE MISMATCH")


# --- pick sample pairs ---------------------------------------------------
def saes_pairs():
    conn_pairs, disc_pairs = [], []
    ids = [s for s in sae if node_of(s)]
    for m in ids:
        for s in ids:
            nm, ns = node_of(m), node_of(s)
            if nm == ns:
                continue
            (conn_pairs if qkd_connected(nm, ns) else disc_pairs).append((m, s))
    return conn_pairs, disc_pairs


conn_pairs, disc_pairs = saes_pairs()
K = 8
conn_sample = conn_pairs[:K]
disc_sample = disc_pairs[:K]

print(f"=== seclevels e2e — QKD components: {len(set(comp.values()))}, "
      f"QKD-connected pairs={len(conn_pairs)}, QKD-disconnected={len(disc_pairs)} ===")

results = []  # (label, ok, detail)


def check(label, ok, detail=""):
    results.append((label, ok, detail))
    print(f"  [{'PASS' if ok else 'FAIL'}] {label}" + ("" if ok else f"  ({detail})"))


# qkd_prefer (default, no extension) round-trip on both classes -> must match
for m, s in conn_sample + disc_sample:
    ok, d = roundtrip(m, s)
    check(f"default(qkd_prefer) {m}->{s} [QKD={qkd_connected(node_of(m),node_of(s))}] match", ok, d)

# no_worry round-trip -> must match
for m, s in (conn_sample[:4] + disc_sample[:4]):
    ok, d = roundtrip(m, s, "no_worry", mandatory=False)
    check(f"no_worry {m}->{s} match", ok, d)

# strict_qkd: 200+match on connected, 4xx on disconnected
for m, s in conn_sample[:6]:
    ok, d = roundtrip(m, s, "strict_qkd", mandatory=True)
    check(f"strict_qkd {m}->{s} (QKD-connected) => match", ok, d)
for m, s in disc_sample[:6]:
    st, data = enc(m, s, "strict_qkd", mandatory=True)
    ok = 400 <= st < 500
    check(f"strict_qkd {m}->{s} (QKD-disconnected) => 4xx", ok, f"got {st}")

npass = sum(1 for _, ok, _ in results if ok)
print(f"=== {npass}/{len(results)} passed ===")
sys.exit(0 if npass == len(results) and results else 1)
