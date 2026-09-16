# Quick start — just the commands

Condensed version. Explanations, the meaning of every field, verification and
troubleshooting: [`../README.md`](../README.md); the ten-minute overview of
the model: [`docs/deployment.md`](../../docs/deployment.md).

Conventions: SDN at `10.0.0.100`; node `N` = `qkc_id N` + `orr_N` + `dkms-N`.
`node.yml` templates in this folder. **One `net-ca` for the whole network**:
`gen-certs.sh` creates the CA the first time and reuses it afterwards, so
issue every cert from the same `certs/` (or copy `net-ca.crt` + `net-ca.key`
there first) — two directories give two CAs and `UnknownIssuer` on every dial.

## 0. Maintainer: publish the images (once per version)

```bash
# from the repo root
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push

# (lab, no registry: local build + move over ssh)
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load
docker save tuusuario/qkc:latest | ssh otra-maquina docker load
```

## On every machine (once)

```bash
curl -fsSL https://get.docker.com | sudo sh
```

## 1. SDN (central operator)

```bash
mkdir sdn && cd sdn
.../docker/gen-certs.sh sdn 10.0.0.100 ./certs   # control_tls (default on): node cert
                                                 # from the federation's net-ca
cp .../docker/compose/sdn.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.sdn.yml node.yml   # almost empty: it carries no topology
docker compose -f sdn.yml pull && docker compose -f sdn.yml up -d
curl -s --cert certs/sdn.crt --key certs/sdn.key --cacert certs/net-ca.crt \
     https://localhost:19002/topology     # mTLS admin: everything at 0; it fills in
                                          # on its own as the other modules boot
```

## 1-bis. quditto — only if you want `qkd` links without hardware

One per **link** (not per node): the QKCs at its two ends point at the same
`kme_url`. With `pqc` links skip this step.

```bash
mkdir quditto-1-2 && cd quditto-1-2
.../docker/gen-certs.sh quditto <IP-anunciable> ./certs   # its ETSI-014 serves mTLS by
                                                          # default (net-ca client certs)
cp .../docker/compose/quditto.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.quditto.yml node.yml   # edit: r0/alpha/distance_km
docker compose -f quditto.yml up -d
curl -s --cert certs/quditto.crt --key certs/quditto.key --cacert certs/net-ca.crt \
     https://localhost:20010/api/v1/keys/1/status   # healthy: stored_key_count > 0
```

## 2. QKC

```bash
mkdir qkc && cd qkc
.../docker/gen-certs.sh qkc-1 <IP-anunciable> ./certs    # control_tls (default on): node
                                                         # cert (announce, admin, handshakes)
cp .../docker/compose/qkc.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.qkc.yml node.yml   # edit: qkc_id + links (neighbours)
docker compose -f qkc.yml pull && docker compose -f qkc.yml up -d
docker compose -f qkc.yml logs -f    # healthy: "qkc.pqc.handshake.established" per neighbour
```

With `type: qkd` also add `r0`/`alpha`/`distance_km` at **both** ends: the QKC
does not use them, it passes them to the SDN to size the edge. With a quditto,
the same ones you configured on it.

## 3. ORR

```bash
mkdir orr && cd orr
.../docker/gen-certs.sh orr_1 <IP-anunciable> ./certs    # its gRPC runs with mTLS by
                                                         # default: node cert (net-ca)
cp .../docker/compose/orr.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.orr.yml node.yml   # edit: ids, qkc_addr, sdn_url;
                                               # peers = optional seed, the SDN
                                               # sends the live list
docker compose -f orr.yml pull && docker compose -f orr.yml up -d
docker compose -f orr.yml logs -f    # healthy: "bootstrap_secret ok" per peer
```

## 4. DKMS

```bash
mkdir dkms && cd dkms
.../docker/gen-certs.sh dkms-1 <IP-anunciable> ./certs   # roots net-ca.* (nodes) and
                                                         # sae-ca.* (SAEs): created once,
                                                         # reused if already there
cp .../docker/compose/dkms.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.dkms.yml node.yml  # edit: node_id, advertise_ip,
                                               # orr_addr, sdn_endpoint, peers,
                                               # sae_bindings (mandatory)
docker compose -f dkms.yml pull && docker compose -f dkms.yml up -d
docker compose -f dkms.yml logs -f   # healthy: 'listening plane="sae"' +
                                     # "generator.state ... ack_pending=0"
```

## A whole node on one machine (qkc+orr+dkms)

```bash
mkdir mi-nodo && cd mi-nodo
cp .../docker/compose/site.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
# node.qkc.yml + node.orr.yml + node.dkms.yml + certs/   (qkc_addr/orr_addr at 127.0.0.1)
docker compose -f site.yml up -d
```

## ETSI-014 smoke test (identical key on two nodes)

```bash
.../docker/gen-certs.sh --sae sae_1 ./certs
.../docker/gen-certs.sh --sae sae_2 ./certs
C=./certs

# sae_1 -> dkms of node 1: ask for a key with sae_2 (note down the key_ID)
curl -s --cacert $C/net-ca.crt --cert $C/sae_1.crt --key $C/sae_1.key \
  -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
  https://10.0.0.11:20005/api/v1/keys/sae_2/enc_keys

# sae_2 -> dkms of node 2: collect the key (it must be the same "key")
curl -s --cacert $C/net-ca.crt --cert $C/sae_2.crt --key $C/sae_2.key \
  -H 'Content-Type: application/json' -d '{"key_IDs":[{"key_ID":"<uuid>"}]}' \
  https://10.0.0.12:20005/api/v1/keys/sae_1/dec_keys
```

## Port cheat sheet (firewall)

| open | on | for |
|------|----|------|
| 20000 | qkc | neighbour QKCs |
| 20002 | qkc | only the SDN |
| 20003 | orr | peer ORRs + its DKMS |
| 20005 | dkms | SAEs |
| 20006 | dkms | peer DKMSs (ETSI-020 + the generator ACKs; 20009 only with the legacy socket ACKs, both ends) |
| 19000 | sdn | the DKMSs (gRPC; the ORR's optional channel does not come up under `control_tls`, the QKC has none) |
| 19002 | sdn | all the QKCs/ORRs/DKMSs (the HTTP admin they register on, `/rate`) |

## Day to day

```bash
docker compose -f <rol>.yml pull && docker compose -f <rol>.yml up -d   # update
docker compose -f <rol>.yml restart                                    # after editing node.yml
docker compose -f <rol>.yml exec <rol> cat /run/cfg/default.toml       # rendered config
```
