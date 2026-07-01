#!/usr/bin/env python3
"""Generate a binaries-only DKMS deployment (no orchestrator / DB / k8s) from a
topology graph, for running the Rust binaries directly on a CESGA Slurm cluster.

Model — "site per node": every graph node is a full site {dkms + orr + qkc}
co-located on one host; every graph edge is one shared quditto; the SDN is a
singleton. This mirrors the EKS orchestrator wiring but emits local config files
instead of k8s manifests.

The topology graph comes from ``tests/cli/topology_builders.py`` (reused as-is).
This script fills everything that graph lacks: per-site IDs, host placement,
programmatic ports, the SDN ``topology_dir`` JSON, full per-binary TOML/configs
(QKC TOML, ORR TOML, DKMS TOML, SDN TOML, quditto CLI specs), the PKI (CA +
per-DKMS server cert + per-SAE client cert), a load-driver SAE assignment, and a
machine-readable launch plan consumed by ``launch.py``.

Single-node (loopback) and multinode (real node IPs) are both supported via
``--hosts``. Output goes to ``--out`` (put it on $LUSTRE: $HOME has an inode quota).
"""
from __future__ import annotations

import argparse
import json
import os
import random
import subprocess
import sys
from pathlib import Path

# Reuse the topology builders verbatim.
_REPO = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(_REPO / "tests" / "cli"))
import topology_builders as tb  # noqa: E402

# ── Port plan ───────────────────────────────────────────────────────────────
# All ports stay BELOW the ephemeral range (32768). A site's block is
# SITE_BASE + idx*SITE_STRIDE; sites on the same node never collide because idx
# is global. Quditto is per-edge in its own range. SDN has a fixed block whose
# http port is grpc+2 (the DKMS derives the SDN admin URL as grpc_port+2).
SITE_BASE = 20000
SITE_STRIDE = 16
QD_BASE = 30000          # quditto per-edge, 30000+edge_idx  (< 32768 for <2768 edges)
SDN_GRPC = 19000
SDN_HTTP = SDN_GRPC + 2  # MUST be grpc+2
SDN_METRICS = 19010


def site_ports(idx: int) -> dict[str, int]:
    b = SITE_BASE + idx * SITE_STRIDE
    return {
        "qkc_peer": b + 0,
        "qkc_local": b + 1,
        "qkc_admin": b + 2,
        "orr_grpc": b + 3,
        "orr_metrics": b + 4,
        "dkms_sae": b + 5,
        "dkms_peer": b + 6,
        "dkms_grpc": b + 7,
        "dkms_metrics": b + 8,
        "dkms_ack": b + 9,
    }


# ── Topology construction ───────────────────────────────────────────────────
def build_graph(args: argparse.Namespace) -> dict:
    t = args.topo
    if t == "star":
        g = tb.build_star(args.per_branch, args.branches)
    elif t == "line":
        g = tb.build_line(args.n)
    elif t == "ring":
        g = tb.build_ring(args.n)
    elif t == "mesh":
        g = tb.build_mesh(args.rows, args.cols)
    elif t == "er":
        g = tb.build_er(args.n, args.degree, seed=args.seed)
    elif t == "ba":
        g = tb.build_barabasi_albert(args.n, args.degree, seed=args.seed)
    elif t == "rgg":
        g = tb.build_rgg(args.n, max_distance_km=args.max_distance_km, avg_degree=args.degree, seed=args.seed)
    elif t == "secoqc":
        g = tb.build_secoqc(args.n, avg_degree=args.degree, seed=args.seed)
    else:
        raise SystemExit(f"unknown topo {t}")
    return g


def node_id_of(uid: str) -> int:
    # uids are "node-<id>"
    return int(uid.split("-", 1)[1])


def adjacency(graph: dict) -> tuple[list[int], dict[int, list[int]], list[tuple[int, int, dict]]]:
    ids = sorted(node_id_of(n["uid"]) for n in graph["nodes"])
    adj: dict[int, list[int]] = {i: [] for i in ids}
    edges: list[tuple[int, int, dict]] = []
    for ln in graph["links"]:
        a = node_id_of(ln["source_uid"])
        b = node_id_of(ln["target_uid"])
        lo, hi = (a, b) if a <= b else (b, a)
        adj[a].append(b)
        adj[b].append(a)
        edges.append((lo, hi, ln))
    for i in ids:
        adj[i] = sorted(set(adj[i]))
    edges.sort(key=lambda e: (e[0], e[1]))
    return ids, adj, edges


# ── PKI (openssl subprocess) ────────────────────────────────────────────────
def run(cmd: list[str]) -> None:
    subprocess.run(cmd, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def gen_pki(tls: Path, dkms_specs: list[dict], sae_specs: list[dict]) -> None:
    tls.mkdir(parents=True, exist_ok=True)
    ca_key, ca_crt = tls / "ca.key", tls / "ca.crt"
    if not ca_crt.exists():
        run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(ca_key)])
        run(["openssl", "req", "-x509", "-new", "-key", str(ca_key), "-days", "365",
             "-subj", "/CN=dkms-cesga-ca", "-out", str(ca_crt)])

    def cert(name: str, cn: str, san: str) -> None:
        crt = tls / f"{name}.crt"
        if crt.exists():
            return
        key = tls / f"{name}.key"
        csr = tls / f"{name}.csr"
        ext = tls / f"{name}.ext"
        run(["openssl", "genpkey", "-algorithm", "RSA", "-pkeyopt", "rsa_keygen_bits:2048", "-out", str(key)])
        run(["openssl", "req", "-new", "-key", str(key), "-subj", f"/CN={cn}", "-out", str(csr)])
        ext.write_text(f"subjectAltName = {san}\n")
        run(["openssl", "x509", "-req", "-in", str(csr), "-CA", str(ca_crt), "-CAkey", str(ca_key),
             "-CAcreateserial", "-out", str(crt), "-days", "365", "-extfile", str(ext)])
        for f in (csr, ext):
            if f.exists():
                f.unlink()

    for d in dkms_specs:
        # server cert: URI SAN for identity (IP-agnostic authz) + IP/DNS for chain validation by clients
        cert(d["dkms_id"], d["dkms_id"], f"URI:dkms://{d['dkms_id']},IP:{d['host_ip']},DNS:localhost")
    for s in sae_specs:
        cert(s["sae_id"], s["sae_id"], f"URI:sae://{s['sae_id']}")
    for f in tls.glob("*.srl"):
        if f.exists():
            f.unlink()


def gen_ec_saes(tls: Path, rt_dir: Path, sae_ids: list[str]) -> None:
    """Issue EC P-256 SAE client certs (combined cert+key PEM) signed by the
    existing CA — fast in-process generation for the many SAEs a round-trip
    campaign needs (openssl-per-cert would be far too slow at thousands)."""
    import datetime
    from cryptography import x509
    from cryptography.x509.oid import NameOID
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.hazmat.backends import default_backend
    be = default_backend()
    ca_crt = x509.load_pem_x509_certificate((tls / "ca.crt").read_bytes(), be)
    ca_key = serialization.load_pem_private_key((tls / "ca.key").read_bytes(), None, be)
    rt_dir.mkdir(parents=True, exist_ok=True)
    nb = datetime.datetime.utcnow() - datetime.timedelta(minutes=5)
    na = datetime.datetime.utcnow() + datetime.timedelta(days=365)
    for sid in sae_ids:
        pem = rt_dir / f"{sid}.pem"
        if pem.exists():
            continue
        key = ec.generate_private_key(ec.SECP256R1(), be)
        cert = (x509.CertificateBuilder()
                .subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, sid)]))
                .issuer_name(ca_crt.subject)
                .public_key(key.public_key())
                .serial_number(x509.random_serial_number())
                .not_valid_before(nb).not_valid_after(na)
                .add_extension(x509.SubjectAlternativeName(
                    [x509.UniformResourceIdentifier(f"sae://{sid}")]), critical=False)
                .sign(ca_key, hashes.SHA256(), be))
        pem.write_bytes(
            key.private_bytes(serialization.Encoding.PEM,
                              serialization.PrivateFormat.PKCS8,
                              serialization.NoEncryption())
            + cert.public_bytes(serialization.Encoding.PEM))


# ── TOML emitters (hand-written; no toml dep needed) ────────────────────────
def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--out", required=True, help="output dir (put on $LUSTRE)")
    ap.add_argument("--topo", required=True,
                    choices=["star", "line", "ring", "mesh", "er", "ba", "rgg", "secoqc"])
    ap.add_argument("--n", type=int, default=4)
    ap.add_argument("--per-branch", type=int, default=1)
    ap.add_argument("--branches", type=int, default=3)
    ap.add_argument("--rows", type=int, default=2)
    ap.add_argument("--cols", type=int, default=2)
    ap.add_argument("--degree", type=float, default=4.0)
    ap.add_argument("--max-distance-km", type=float, default=30.0)
    ap.add_argument("--seed", type=int, default=42)
    ap.add_argument("--hosts", default="127.0.0.1",
                    help="comma-separated host IPs to spread sites across (round-robin); "
                         "default '127.0.0.1' = single-node loopback")
    ap.add_argument("--saes-per-dkms", type=int, default=1)
    ap.add_argument("--pairs", type=int, default=0,
                    help="round-trip campaign: generate P SAE pairs (2P EC certs) cross-node + "
                         "roundtrip_pairs.json (consumed by roundtrip.py). 0 = no round-trip.")
    ap.add_argument("--key-bits", type=int, default=256, help="QKD key size (QKC links + quditto)")
    ap.add_argument("--pqc-distance-threshold-km", type=float, default=0.0,
                    help="edges with distance_km >= threshold become PQC links (no quditto; the two "
                         "QKCs run an ML-KEM handshake and derive keys in-process). Mirrors the real "
                         "motivation: long links can't do QKD. 0 = disabled (all edges QKD).")
    ap.add_argument("--pqc-fraction", type=float, default=0.0,
                    help="when --pqc-distance-threshold-km is 0: make a FRACTION (0..1) of edges PQC. "
                         "Default (--pqc-seed 0): deterministic, the longest fraction by descending "
                         "distance then (lo,hi). With --pqc-seed > 0: a uniform RANDOM subset. 0 = disabled.")
    ap.add_argument("--pqc-seed", type=int, default=0,
                    help="when > 0, select the --pqc-fraction edges as a reproducible RANDOM subset "
                         "(random.Random(seed).sample) instead of the deterministic longest-distance rule. "
                         "Keep --seed fixed to pin ONE base topology and vary only the PQC subset. 0 = "
                         "deterministic (backward-compatible).")
    ap.add_argument("--pqc-suite", default="ml-kem-768",
                    help="ML-KEM parameter set for PQC links (ml-kem-512|768|1024)")
    ap.add_argument("--pqc-rekey-keys", type=int, default=1000,
                    help="PQC link re-keying: rotate the ML-KEM secret every N emitted keys "
                         "(0 = no volume trigger). Default 1000. (0 + --pqc-rekey-secs 0 = single secret)")
    ap.add_argument("--pqc-rekey-secs", type=int, default=3600,
                    help="PQC link re-keying: rotate the secret every T seconds (max age; 0 = no time "
                         "trigger). Default 3600. Rotation fires on max(keys, secs).")
    ap.add_argument("--pqc-rekey-lookahead", type=int, default=2,
                    help="PQC link re-keying: epochs pre-established ahead of the active one so "
                         "rotation is latency-free. Default 2.")
    ap.add_argument("--qd-r0", type=float, default=2000.0)
    ap.add_argument("--qd-alpha", type=float, default=0.2)
    ap.add_argument("--qd-max-buffer", type=int, default=65536)
    ap.add_argument("--buffer-cap", type=int, default=10000, help="DKMS buffer.capacity_per_peer")
    ap.add_argument("--fill-rate", type=float, default=0.0,
                    help="DKMS generator.default_fill_rate_keys_per_s — floor fill rate when the "
                         "SDN hasn't assigned a rate (decouples buffer fill from the slow SDN LP at scale)")
    ap.add_argument("--fill-cap", type=float, default=0.0,
                    help="DKMS generator.max_fill_rate_keys_per_s — CAP on the effective fill rate "
                         "(min(max(sdn,floor),cap)). At N=20 the SDN assigns ~55 keys/s/buffer so a "
                         "feasible client can never saturate; a cap below λ reproduces token-bucket "
                         "backpressure deterministically, topology-independent. 0 = no cap.")
    ap.add_argument("--deliver-queue", type=int, default=65536, help="ORR deliver_queue_capacity")
    ap.add_argument("--max-hops", type=int, default=1, help="DKMS southbound.default_max_hops (1 = E2E PQC)")
    ap.add_argument("--security-level", default="qkd_prefer",
                    choices=["strict_qkd", "qkd_prefer", "no_worry"],
                    help="DKMS default_security_level when a request omits one (per-request "
                         "ETSI extensions still override). qkd_prefer = QKD if a QKD path exists, else PQC.")
    ap.add_argument("--mcf-period-ms", type=int, default=5000)
    ap.add_argument("--binaries", default=str(Path(os.environ.get("LUSTRE", "")) / "dkms-build/target/release"),
                    help="dir with the 5 release binaries")
    args = ap.parse_args()

    out = Path(args.out).resolve()
    hosts = [h.strip() for h in args.hosts.split(",") if h.strip()]
    bindir = Path(args.binaries).resolve()
    tls = out / "tls"

    graph = build_graph(args)
    ids, adj, edges = adjacency(graph)
    N = len(ids)
    idx_of = {nid: i for i, nid in enumerate(ids)}      # node_id -> global site index
    host_of = {nid: hosts[i % len(hosts)] for i, nid in enumerate(ids)}
    sdn_ip = hosts[0]

    # Classify which edges are PQC (no quditto, ML-KEM handshake in QKC).
    # Threshold takes precedence over fraction; both off → all QKD.
    def _edge_dist(ln: dict) -> float:
        return float(ln.get("distance_km", 0) or 0)

    pqc_edges: set[tuple[int, int]] = set()
    if args.pqc_distance_threshold_km and args.pqc_distance_threshold_km > 0:
        for (a, b, ln) in edges:
            if _edge_dist(ln) >= args.pqc_distance_threshold_km:
                pqc_edges.add((a, b))
    elif args.pqc_fraction and args.pqc_fraction > 0:
        frac = min(max(args.pqc_fraction, 0.0), 1.0)
        n_pqc = round(len(edges) * frac)
        if args.pqc_seed > 0:
            # Reproducible RANDOM subset: pin ONE base topology with a fixed
            # --seed and vary only which edges are PQC across --pqc-seed values.
            chosen = random.Random(args.pqc_seed).sample(edges, n_pqc)
        else:
            # Deterministic: the longest fraction by descending distance.
            chosen = sorted(edges, key=lambda e: (-_edge_dist(e[2]), e[0], e[1]))[:n_pqc]
        for (a, b, _ln) in chosen:
            pqc_edges.add((a, b))

    # Per-edge quditto: placed on the lower-id endpoint's host. PQC edges
    # carry the same metadata for record-keeping but spawn no quditto.
    edge_qd: dict[tuple[int, int], dict] = {}
    for e_idx, (a, b, ln) in enumerate(edges):
        edge_qd[(a, b)] = {
            "port": QD_BASE + e_idx,
            "host_ip": host_of[a],
            "r0": ln.get("quditto_rate_r0", args.qd_r0),
            "alpha": ln.get("quditto_rate_alpha", args.qd_alpha),
            "distance": ln.get("distance_km", 0),
            "max_buffer": args.qd_max_buffer,
            "is_pqc": (a, b) in pqc_edges,
        }

    def qd_for(i: int, j: int) -> dict:
        return edge_qd[(i, j) if i <= j else (j, i)]

    # ── SAE specs + global binding map ──────────────────────────────────────
    # Round-trip campaign (--pairs P): P pairs, each master+slave on DIFFERENT
    # nodes (so the round-trip crosses the network). 2P EC SAE certs. Else the
    # regular M-per-DKMS RSA SAEs.
    sae_specs: list[dict] = []
    roundtrip_pairs: list[dict] = []
    rt_dir = tls / "rt"
    dkms_specs = [{"dkms_id": f"dkms-{nid}", "host_ip": host_of[nid]} for nid in ids]

    if args.pairs > 0:
        rt_sae_ids: list[str] = []
        for p in range(args.pairs):
            mnode = ids[p % N]
            # slave on a different host when possible, else a different node
            cand = [j for j in ids if host_of[j] != host_of[mnode]] or [j for j in ids if j != mnode]
            snode = cand[(p // max(1, N)) % len(cand)] if cand else mnode
            msae, ssae = f"sae_rtm_{p}", f"sae_rts_{p}"
            rt_sae_ids += [msae, ssae]
            for sid, nid in ((msae, mnode), (ssae, snode)):
                sae_specs.append({"sae_id": sid, "home": nid, "dkms_id": f"dkms-{nid}"})
            mp = site_ports(idx_of[mnode]); sp = site_ports(idx_of[snode])
            roundtrip_pairs.append({
                "pair_id": p, "master_sae": msae, "slave_sae": ssae,
                "master_host_id": mnode, "slave_host_id": snode,
                "enc_url": f"https://{host_of[mnode]}:{mp['dkms_sae']}/api/v1/keys/{ssae}/enc_keys",
                "dec_url": f"https://{host_of[snode]}:{sp['dkms_sae']}/api/v1/keys/{msae}/dec_keys",
                "master_pem": str(rt_dir / f"{msae}.pem"), "slave_pem": str(rt_dir / f"{ssae}.pem"),
            })
        sae_bindings = {s["sae_id"]: s["dkms_id"] for s in sae_specs}
        gen_pki(tls, dkms_specs, [])          # CA + DKMS server certs only
        gen_ec_saes(tls, rt_dir, rt_sae_ids)  # 2P EC SAE certs (fast)
    else:
        for nid in ids:
            for k in range(args.saes_per_dkms):
                sae_specs.append({"sae_id": f"sae_{nid}_{k}", "home": nid, "dkms_id": f"dkms-{nid}"})
        sae_bindings = {s["sae_id"]: s["dkms_id"] for s in sae_specs}
        gen_pki(tls, dkms_specs, sae_specs)

    # ── topology_dir for the SDN ────────────────────────────────────────────
    topo = out / "topology"
    for sub in ("QKC", "ORR", "DKMS", "SAE"):
        (topo / sub).mkdir(parents=True, exist_ok=True)
    for nid in ids:
        p = site_ports(idx_of[nid])
        kmes = []
        for j in adj[nid]:
            qd = qd_for(nid, j)
            kmes.append({
                "neighbor_qkc_id": str(j),
                "channel": {
                    "distance": qd["distance"],
                    "quditto_rate_r0": qd["r0"],
                    "quditto_rate_alpha": qd["alpha"],
                    "quditto_max_buffer_size": qd["max_buffer"],
                    "link_type": "pqc" if qd["is_pqc"] else "qkd",
                },
            })
        (topo / "QKC" / f"qkc-{nid}.json").write_text(json.dumps({
            "id": str(nid),
            "host": {"id": nid, "ip": host_of[nid], "port": p["qkc_admin"]},
            "kmes": kmes,
        }, indent=2))
        (topo / "ORR" / f"orr_{nid}.json").write_text(json.dumps({
            "id": f"orr_{nid}", "qkc_id": str(nid),
            "host": {"id": 200 + nid, "ip": host_of[nid], "port": p["orr_grpc"]},
        }, indent=2))
        (topo / "DKMS" / f"dkms-{nid}.json").write_text(json.dumps({
            "id": f"dkms-{nid}", "orr_id": f"orr_{nid}",
            "host": {"id": 300 + nid, "ip": host_of[nid], "port": p["dkms_sae"]},
        }, indent=2))
    # Per-SAE topology JSON — REQUIRED: with the SDN up the DKMS uses
    # SdnSaeResolver (GetSaeBinding over the topology), so a SAE absent from
    # topology_dir/SAE/ → enc_keys 404. (Static [sae_bindings] is only a
    # fallback when the SDN is unreachable.) Cheap lookup, independent of the LP.
    for s in sae_specs:
        (topo / "SAE" / f"{s['sae_id']}.json").write_text(json.dumps({
            "id": s["sae_id"], "dkms_id": s["dkms_id"],
        }, indent=2))

    # ── SDN config ──────────────────────────────────────────────────────────
    write(out / "sdn" / "default.toml",
          f'node_id = "sdn"\n'
          f'grpc_addr = "0.0.0.0:{SDN_GRPC}"\n'
          f'http_addr = "0.0.0.0:{SDN_HTTP}"\n'
          f'metrics_addr = "0.0.0.0:{SDN_METRICS}"\n'
          f'topology_dir = "{topo}"\n'
          f'default_policy = "shortest_hops"\n'
          f'mcf_period_ms = {args.mcf_period_ms}\n'
          f'push_debounce_ms = 100\n')

    # ── Per-site QKC / ORR / DKMS configs ───────────────────────────────────
    for nid in ids:
        p = site_ports(idx_of[nid])
        hip = host_of[nid]

        # QKC (--config TOML; no env layer)
        links = ""
        for j in adj[nid]:
            pj = site_ports(idx_of[j])
            qd = qd_for(nid, j)
            # neighbor_peer_addr siempre (los PQC usan ese TCP para el
            # handshake ML-KEM); quditto_url SOLO en QKD.
            links += (f'\n[[links]]\n'
                      f'neighbor_id = {j}\n'
                      f'neighbor_peer_addr = "{host_of[j]}:{pj["qkc_peer"]}"\n')
            if qd["is_pqc"]:
                links += (f'link_type = "pqc"\n'
                          f'pqc_suite = "{args.pqc_suite}"\n'
                          f'pqc_rekey_keys = {args.pqc_rekey_keys}\n'
                          f'pqc_rekey_secs = {args.pqc_rekey_secs}\n'
                          f'pqc_rekey_lookahead = {args.pqc_rekey_lookahead}\n')
            else:
                links += f'quditto_url = "http://{qd["host_ip"]}:{qd["port"]}"\n'
            links += f'key_size_bits = {args.key_bits}\n'
        write(out / "sites" / f"site-{nid}" / "qkc.toml",
              f'qkc_id = {nid}\n'
              f'peer_listen = "0.0.0.0:{p["qkc_peer"]}"\n'
              f'local_listen = "0.0.0.0:{p["qkc_local"]}"\n'
              f'admin_http = "0.0.0.0:{p["qkc_admin"]}"\n' + links)

        # ORR (CONFIG_DIR/default.toml)
        peers = "".join(f'orr_{j} = {j}\n' for j in ids if j != nid)
        paddr = "".join(
            f'orr_{j} = "http://{host_of[j]}:{site_ports(idx_of[j])["orr_grpc"]}"\n'
            for j in ids if j != nid)
        write(out / "sites" / f"site-{nid}" / "orr" / "default.toml",
              f'orr_id = "orr_{nid}"\n'
              f'qkc_id = {nid}\n'
              f'qkc_local_addr = "127.0.0.1:{p["qkc_local"]}"\n'
              f'grpc_addr = "0.0.0.0:{p["orr_grpc"]}"\n'
              f'sdn_url = "http://{sdn_ip}:{SDN_GRPC}"\n'
              f'metrics_addr = "0.0.0.0:{p["orr_metrics"]}"\n'
              f'default_max_hops = 0\n'
              f'deliver_queue_capacity = {args.deliver_queue}\n'
              f'\n[peers]\n{peers}'
              f'\n[peer_grpc_addrs]\n{paddr}'
              f'\n[peer_pubkeys]\n')

        # DKMS (CONFIG_DIR/default.toml)
        peers_toml = ""
        for j in ids:
            if j == nid:
                continue
            pj = site_ports(idx_of[j])
            peers_toml += (f'\n[peers.dkms-{j}]\n'
                           f'endpoint = "https://{host_of[j]}:{pj["dkms_peer"]}"\n'
                           f'transport = "orr"\n'
                           f'orr_id = "orr_{j}"\n')
        binds = "".join(f'{sid} = "{did}"\n' for sid, did in sorted(sae_bindings.items()))
        write(out / "sites" / f"site-{nid}" / "dkms" / "default.toml",
              f'node_id = "dkms-{nid}"\n'
              f'default_security_level = "{args.security_level}"\n\n'
              f'[listen]\n'
              f'sae_addr = "0.0.0.0:{p["dkms_sae"]}"\n'
              f'peer_addr = "0.0.0.0:{p["dkms_peer"]}"\n'
              f'grpc_addr = "0.0.0.0:{p["dkms_grpc"]}"\n'
              f'metrics_addr = "0.0.0.0:{p["dkms_metrics"]}"\n\n'
              f'[generator]\n'
              f'ack_socket_addr = "0.0.0.0:{p["dkms_ack"]}"\n'
              f'ack_advertised_endpoint = "{hip}:{p["dkms_ack"]}"\n'
              f'rate_refresh_ms = 1000\n'
              f'default_fill_rate_keys_per_s = {args.fill_rate}\n'
              f'max_fill_rate_keys_per_s = {args.fill_cap}\n\n'
              f'[tls]\n'
              f'cert_path = "{tls}/dkms-{nid}.crt"\n'
              f'key_path = "{tls}/dkms-{nid}.key"\n'
              f'sae_client_ca = "{tls}/ca.crt"\n'
              f'peer_dkms_ca = "{tls}/ca.crt"\n\n'
              f'[southbound]\n'
              f'sdn_endpoint = "http://{sdn_ip}:{SDN_GRPC}"\n'
              f'qkc_endpoint = "http://127.0.0.1:1"\n'
              f'orr_endpoint = "http://127.0.0.1:{p["orr_grpc"]}"\n'
              f'connect_timeout_ms = 1500\n'
              f'rpc_timeout_ms = 5000\n'
              f'default_max_hops = {args.max_hops}\n\n'
              f'[buffer]\n'
              f'capacity_per_peer = {args.buffer_cap}\n'
              f'refill_low_watermark = 1024\n'
              f'refill_batch = 256\n\n'
              f'[sae]\n'
              f'default_rate_keys_per_sec = 200000\n'
              f'default_burst_keys = 400000\n'
              f'token_unit_bytes = 32\n\n'
              f'[sae_bindings]\n{binds}'
              f'{peers_toml}')

    # ── Launch plan ─────────────────────────────────────────────────────────
    procs: list[dict] = []
    procs.append({
        "name": "sdn", "role": "sdn", "phase": 0, "host_ip": sdn_ip, "host_index": 0,
        "cmd": [str(bindir / "sdn")],
        "env": {"CONFIG_DIR": str(out / "sdn")},
        "ready": {"type": "http", "url": f"http://{sdn_ip}:{SDN_HTTP}/topology"},
    })
    for (a, b, ln) in edges:
        qd = edge_qd[(a, b)]
        if qd["is_pqc"]:
            continue  # PQC edge: no quditto process (keys derived in-QKC)
        procs.append({
            "name": f"qd-{a}-{b}", "role": "quditto", "phase": 1, "host_ip": qd["host_ip"],
            "host_index": idx_of[a],
            "cmd": [str(bindir / "quditto"), "--listen", f"0.0.0.0:{qd['port']}",
                    "--r0", str(qd["r0"]), "--alpha", str(qd["alpha"]),
                    "--distance", str(qd["distance"]), "--max-buffer", str(qd["max_buffer"]),
                    "--key-size-bits", str(args.key_bits)],
            "env": {},
            "ready": {"type": "http", "url": f"http://{qd['host_ip']}:{qd['port']}/healthz"},
        })
    for nid in ids:
        p = site_ports(idx_of[nid])
        hip = host_of[nid]
        procs.append({
            "name": f"qkc-{nid}", "role": "qkc", "phase": 1, "host_ip": hip, "host_index": idx_of[nid],
            "cmd": [str(bindir / "qkc"), "--config", str(out / "sites" / f"site-{nid}" / "qkc.toml")],
            "env": {},
            "ready": {"type": "http", "url": f"http://{hip}:{p['qkc_admin']}/healthz"},
        })
    for nid in ids:
        p = site_ports(idx_of[nid])
        procs.append({
            "name": f"orr-{nid}", "role": "orr", "phase": 2, "host_ip": host_of[nid], "host_index": idx_of[nid],
            "cmd": [str(bindir / "orr")],
            "env": {"CONFIG_DIR": str(out / "sites" / f"site-{nid}" / "orr")},
            "ready": {"type": "tcp", "host": host_of[nid], "port": p["orr_grpc"]},
        })
    for nid in ids:
        p = site_ports(idx_of[nid])
        procs.append({
            "name": f"dkms-{nid}", "role": "dkms", "phase": 3, "host_ip": host_of[nid], "host_index": idx_of[nid],
            "cmd": [str(bindir / "dkms")],
            "env": {"CONFIG_DIR": str(out / "sites" / f"site-{nid}" / "dkms")},
            "ready": {"type": "tcp", "host": host_of[nid], "port": p["dkms_sae"]},
        })

    # ── SAE assignment for the simple load driver (smoke.py / load.py) ───────
    # Skipped for round-trip campaigns (--pairs): those use roundtrip_pairs.json.
    sae_assign = []
    if args.pairs == 0:
        for s in sae_specs:
            s_host = host_of[s["home"]]
            cross = [o for o in sae_specs if host_of[o["home"]] != s_host]
            same = [o for o in sae_specs if o["home"] != s["home"]]
            pool = cross if cross else same
            slave = pool[s["home"] % len(pool)] if pool else s
            p = site_ports(idx_of[s["home"]])
            sae_assign.append({
                "sae_id": s["sae_id"], "home_dkms": s["dkms_id"],
                "url": f"https://{host_of[s['home']]}:{p['dkms_sae']}",
                "slave_sae": slave["sae_id"],
                "cert": f"{tls}/{s['sae_id']}.crt", "key": f"{tls}/{s['sae_id']}.key", "ca": f"{tls}/ca.crt",
            })
    else:
        (out / "roundtrip_pairs.json").write_text(json.dumps(roundtrip_pairs, indent=2))

    plan = {
        "meta": {"topo": args.topo, "N": N, "edges": len(edges),
                 "edge_list": sorted([[a, b] for (a, b, _ln) in edges]),
                 "pqc_edges": sorted([list(e) for e in pqc_edges]), "hosts": hosts,
                 "saes_per_dkms": args.saes_per_dkms, "key_bits": args.key_bits,
                 "out": str(out), "binaries": str(bindir)},
        "ids": ids, "host_of": {str(k): v for k, v in host_of.items()},
        "ports": {str(nid): site_ports(idx_of[nid]) for nid in ids},
        "sdn": {"ip": sdn_ip, "grpc": SDN_GRPC, "http": SDN_HTTP, "metrics": SDN_METRICS},
        "procs": procs,
        "sae_assign": sae_assign,
    }
    (out / "plan.json").write_text(json.dumps(plan, indent=2))

    n_pqc = len(pqc_edges)
    print(f"[gen] topo={args.topo} N={N} edges={len(edges)} (qkd={len(edges) - n_pqc} pqc={n_pqc}) hosts={hosts}")
    print(f"[gen] sites={N} qudittos={len(edges) - n_pqc} saes={len(sae_specs)} procs={len(procs)}")
    print(f"[gen] out={out}")
    print(f"[gen] plan={out / 'plan.json'}")


if __name__ == "__main__":
    main()
