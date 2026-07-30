# Quick start — solo los comandos

Versión condensada. Explicaciones, significado de cada campo, verificación y
troubleshooting: [`../README.md`](../README.md).

Convenciones: SDN en `10.0.0.100`; nodo `N` = `qkc_id N` + `orr_N` + `dkms-N`.
Plantillas de `node.yml` en esta carpeta.

## 0. Mantenedor: publicar imágenes (una vez por versión)

```bash
# desde la raíz del repo
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push

# (lab, sin registry: build local + mover por ssh)
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load
docker save tuusuario/qkc:latest | ssh otra-maquina docker load
```

## En cada máquina (una vez)

```bash
curl -fsSL https://get.docker.com | sudo sh
```

## 1. SDN (operador central)

```bash
mkdir sdn && cd sdn
cp .../docker/compose/sdn.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/topology.example.yml node.yml   # editar: nodes/links/saes
docker compose -f sdn.yml pull && docker compose -f sdn.yml up -d
docker compose -f sdn.yml logs -f    # sano: "forwarding push done ... qkcs_err=0"
```

## 1-bis. quditto — solo si quieres enlaces `qkd` sin hardware

Uno por **enlace** (no por nodo): los QKC de sus dos extremos apuntan al mismo
`kme_url`. Con enlaces `pqc` sáltate este paso.

```bash
mkdir quditto-1-2 && cd quditto-1-2
cp .../docker/compose/quditto.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.quditto.yml node.yml   # editar: r0/alpha/distance_km
docker compose -f quditto.yml up -d
curl -s http://localhost:20010/api/v1/keys/1/status   # sano: stored_key_count > 0
```

## 2. QKC

```bash
mkdir qkc && cd qkc
cp .../docker/compose/qkc.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.qkc.yml node.yml   # editar: qkc_id + links (vecinos)
docker compose -f qkc.yml pull && docker compose -f qkc.yml up -d
docker compose -f qkc.yml logs -f    # sano: "qkc.pqc.handshake.established" por vecino
```

Con `type: qkd` los QKC solo llevan el `kme_url`. Los `r0`/`alpha`/`distance_km`
del enlace van en el `topology.yml` de la SDN y deben coincidir con los del
quditto.

## 3. ORR

```bash
mkdir orr && cd orr
cp .../docker/compose/orr.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.orr.yml node.yml   # editar: ids, qkc_addr, sdn_url,
                                               # peers = TODOS los ORR de la red
docker compose -f orr.yml pull && docker compose -f orr.yml up -d
docker compose -f orr.yml logs -f    # sano: "bootstrap_secret ok" por peer
```

## 4. DKMS

```bash
mkdir dkms && cd dkms
.../docker/gen-certs.sh dkms-1 <IP-anunciable> ./certs   # CA común: se reutiliza
                                                         # si ya hay ca.crt/ca.key
cp .../docker/compose/dkms.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.dkms.yml node.yml  # editar: node_id, advertise_ip,
                                               # orr_addr, sdn_endpoint, peers
docker compose -f dkms.yml pull && docker compose -f dkms.yml up -d
docker compose -f dkms.yml logs -f   # sano: 'listening plane="sae"' +
                                     # "generator.state ... ack_pending=0"
```

## Nodo completo en una máquina (qkc+orr+dkms)

```bash
mkdir mi-nodo && cd mi-nodo
cp .../docker/compose/site.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
# node.qkc.yml + node.orr.yml + node.dkms.yml + certs/   (qkc_addr/orr_addr en 127.0.0.1)
docker compose -f site.yml up -d
```

## Smoke test ETSI-014 (clave idéntica en dos nodos)

```bash
.../docker/gen-certs.sh --sae sae_1 ./certs
.../docker/gen-certs.sh --sae sae_2 ./certs
C=./certs

# sae_1 -> dkms del nodo 1: pedir clave con sae_2 (apunta el key_ID)
curl -s --cacert $C/ca.crt --cert $C/sae_1.crt --key $C/sae_1.key \
  -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
  https://10.0.0.11:20005/api/v1/keys/sae_2/enc_keys

# sae_2 -> dkms del nodo 2: recoger la clave (debe ser la misma "key")
curl -s --cacert $C/ca.crt --cert $C/sae_2.crt --key $C/sae_2.key \
  -H 'Content-Type: application/json' -d '{"key_IDs":[{"key_ID":"<uuid>"}]}' \
  https://10.0.0.12:20005/api/v1/keys/sae_1/dec_keys
```

## Chuleta de puertos (firewall)

| abre | en | para |
|------|----|------|
| 20000 | qkc | QKC vecinos |
| 20002 | qkc | solo la SDN |
| 20003 | orr | ORR peers + su DKMS |
| 20005 | dkms | SAEs |
| 20006, 20009 | dkms | DKMS peers |
| 19000 | sdn | todos los ORR/DKMS |

## Día a día

```bash
docker compose -f <rol>.yml pull && docker compose -f <rol>.yml up -d   # actualizar
docker compose -f <rol>.yml restart                                    # tras editar node.yml
docker compose -f <rol>.yml exec <rol> cat /run/cfg/default.toml       # config renderizada
```
