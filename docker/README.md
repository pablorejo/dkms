# Despliegue por contenedores (multi-institución, manual)

Imágenes Docker públicas, una por módulo (`qkc`, `orr`, `dkms`, `sdn`), para que
**cada institución despliegue lo suyo de forma autónoma**: `docker compose up`
rellenando solo un `node.yml` corto. Sin orquestador central.

- **Sin QKD real** → el QKC usa enlaces **PQC** ("QKD simulado por PQC",
  ML-KEM-768). No hace falta quditto.
- **Con QKD real** → el enlace del QKC apunta al **KME ETSI-014** del hardware.

El modelo de red: una **SDN central única** (la mantiene el operador) que ve
toda la topología, y en cada nodo/institución un trío **QKC + ORR + DKMS**
(en la misma máquina o repartidos). Flujo validado end-to-end el 2026-07-02
sobre 4 máquinas (ver `tests/results/proxmox-docker-smoke/`).

## Requisitos

- Docker + `docker compose` en cada máquina (Debian, Raspberry Pi OS, etc.).
  Instalación rápida: `curl -fsSL https://get.docker.com | sudo sh`.
- Conectividad entre las máquinas que deben hablarse (ver tabla de puertos).
  IPs públicas, VPN (WireGuard) o rutas acordadas entre instituciones.
- Para el DKMS: certificados firmados por una **CA común** a toda la red
  (sección TLS más abajo).

## Puertos por defecto (host network)

| módulo | puerto | quién se conecta |
|--------|--------|------------------|
| qkc  | 20000 (peer) | los QKC vecinos (binary TCP) |
| qkc  | 20001 (local) | su ORR (misma máquina o remoto) |
| qkc  | 20002 (admin) | **solo la SDN** (push de forwarding) |
| orr  | 20003 (grpc) | los ORR peers y su DKMS |
| orr  | 20004 (metrics) | Prometheus (opcional) |
| dkms | 20005 (sae) | los SAEs (ETSI-014, mTLS) |
| dkms | 20006 (peer) | los otros DKMS (ETSI-020, mTLS) |
| dkms | 20007 (grpc) | interno |
| dkms | 20008 (metrics) | Prometheus (opcional) |
| dkms | 20009 (ack) | los otros DKMS (ACKs del generator, TCP plano) |
| sdn  | 19000 (grpc) | todos los ORR y DKMS |
| sdn  | 19002 (http) | admin |
| sdn  | 19010 (metrics) | Prometheus (opcional) |

Override con `ports:` en el `node.yml` solo si una máquina corre varios nodos
del mismo rol. Firewall recomendado: `admin` del QKC solo hacia la SDN; `peer`
del QKC/DKMS y `ack` solo entre las instituciones que se enlazan.

## Paso 0 (mantenedor): construir y publicar las imágenes

```bash
# desde la raíz del repo, con buildx configurado para multi-arch
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
```

Construye `qkc/orr/dkms/sdn` para `linux/amd64` + `linux/arm64` (raspi) y las
sube a Docker Hub. El build compila los 4 binarios en una sola pasada (etapa
compartida). Nota: `aws-lc-sys` (dep de rustls) exige **gcc-12** — la base
`rust:1.88-bookworm` ya lo trae. arm64 va por emulación QEMU (lento) o runner
ARM nativo.

Prueba local de una arch sin push:

```bash
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load
```

Sin registry (lab): `docker save tuusuario/qkc | ssh otra-maquina docker load`.

## Estructura común a todos los módulos

Cada módulo se lanza igual: un directorio con **3 ficheros** (+ certs si es DKMS).

```
mi-modulo/
├── <rol>.yml      # el compose del rol (cópialo de docker/compose/)
├── .env           # IMAGE_PREFIX=<namespace de Docker Hub>
└── node.yml       # LO ÚNICO que se edita (plantillas en docker/examples/)
```

El entrypoint del contenedor convierte el `node.yml` en la config real del
binario Rust (`render_config.py`); la institución no toca TOML. Comandos
idénticos para los 4 roles:

```bash
docker compose -f <rol>.yml pull      # baja la imagen
docker compose -f <rol>.yml up -d     # arranca (restart automático)
docker compose -f <rol>.yml logs -f   # ver estado
```

**Orden de arranque recomendado**: SDN primero, luego el resto en cualquier
orden (QKC → ORR → DKMS si quieres logs limpios). No es crítico: todos los
módulos reintentan la conexión (el DKMS reintenta la SDN 30×1 s, los ORR
re-bootstrapean, la SDN re-pushea el forwarding cada tick).

En los ejemplos siguientes: SDN en `10.0.0.100`, nodo 1 en `10.0.0.11`,
nodo 2 en `10.0.0.12`. Sustituye por tus IPs reales.

---

## 1. SDN (operador central)

La SDN es el único módulo cuya config no es "local": su `node.yml` es la
**topología global** de la red. La mantiene el operador central; cada
institución le comunica (fuera de banda) las IPs de sus módulos.

**Paso 1 — directorio y ficheros:**

```bash
mkdir sdn && cd sdn
cp .../docker/compose/sdn.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/topology.example.yml node.yml
```

**Paso 2 — editar `node.yml`** (la topología):

```yaml
nodes:                              # id de nodo -> IP de cada módulo
  1: { qkc: "10.0.0.11", orr: "10.0.0.11", dkms: "10.0.0.11" }
  2: { qkc: "10.0.0.12", orr: "10.0.0.12", dkms: "10.0.0.12" }

links:                              # enlaces del backbone QKC
  - { a: 1, b: 2, type: pqc }      # pqc | qkd (r0/alpha/distance_km opcionales)

saes:                               # binding SAE -> nodo (opcional)
  - { id: "sae_1", node: 1 }
  - { id: "sae_2", node: 2 }
```

Si un nodo reparte sus módulos en varias máquinas, pon la IP de cada uno
(admite `ip:puerto` si hay override de puertos).

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f sdn.yml up -d
docker compose -f sdn.yml logs -f
```

Logs sanos:

```
sdn::service: forwarding push done ... qkcs_ok=<nº de QKCs> qkcs_err=0
sdn::service: MCMCF-λ recomputed n_commodities=... n_edges=...
```

`qkcs_err>0` es normal mientras los QKC aún no están arriba; se recupera solo
al siguiente tick.

**Alta de una institución nueva**: añadir su nodo y sus enlaces al `node.yml`
y `docker compose -f sdn.yml restart` (la topología se carga en el boot).

---

## 2. QKC

El QKC es la capa de material de clave: enlaza con sus vecinos por PQC o QKD
real. No necesita saber nada de la SDN — es la SDN quien le empuja la tabla de
forwarding a su puerto admin (20002).

**Paso 1 — directorio y ficheros:**

```bash
mkdir qkc && cd qkc
cp .../docker/compose/qkc.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.qkc.yml node.yml
```

**Paso 2 — editar `node.yml`**: tu id numérico y un bloque por vecino.

```yaml
qkc_id: 1

links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"   # IP (o IP:puerto) del QKC vecino
    type: pqc                    # sin hardware QKD
  # con nodo QKD real (ETSI-014):
  # - neighbor_id: 3
  #   neighbor_addr: "10.0.0.13"
  #   type: qkd
  #   kme_url: "https://mi-kme:443"
```

`key_size_bits` (default 256) **debe coincidir en ambos extremos** del enlace.
Ajustes PQC opcionales por enlace: `pqc_suite`, `pqc_rekey_keys`,
`pqc_rekey_secs`, `pqc_rekey_lookahead`.

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f qkc.yml up -d && docker compose -f qkc.yml logs -f
```

Logs sanos (con el vecino ya arriba):

```
qkc::pqc_handshake: qkc.pqc.handshake.established me=1 peer=2 ...
qkc::keystore: keystore.levels peer=2 enc=... dec=... misses=0
```

---

## 3. ORR

El ORR es el enrutador de material entre nodos: habla con **su** QKC (el del
mismo nodo), con la SDN y con los ORR de los demás nodos.

**Paso 1 — directorio y ficheros:**

```bash
mkdir orr && cd orr
cp .../docker/compose/orr.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.orr.yml node.yml
```

**Paso 2 — editar `node.yml`**:

```yaml
orr_id: "orr_1"                    # convención: orr_<id de nodo>
qkc_id: 1                          # el nodo al que pertenece

# Dónde está SU QKC: localhost si co-locado, IP si en otra máquina.
qkc_addr: "127.0.0.1:20001"

sdn_url: "http://10.0.0.100:19000"

peers:                             # un par de entradas por cada OTRO ORR
  orr_2: 2                         #   orr_id -> qkc_id
peer_grpc_addrs:
  orr_2: "http://10.0.0.12:20003"  #   orr_id -> URL gRPC
```

**Importante**: pon en `peers` **todos** los ORR de la red con cuyo DKMS se
intercambiarán claves, no solo los vecinos físicos — el bootstrap PQC
ORR↔ORR es E2E (max_hops=1) e independiente de la topología de enlaces.

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f orr.yml up -d && docker compose -f orr.yml logs -f
```

Logs sanos:

```
orr::bootstrap: orr.peer_pubkey bootstrap ok local=orr_1 peer=orr_2 suite=ml-kem-768
orr::bootstrap: orr.bootstrap bootstrap_secret ok ...
orr::grpc_server: orr.stream_deliveries subscribed subscriber=dkms-dkms-1   # cuando su DKMS conecte
```

Gotcha conocido: si **reinicias solo un ORR**, los peers se quedan con el
`master_secret` viejo ("sin master_secret" en bucle en el nuevo). Reinicia el
conjunto (o al menos los ORR peers) hasta que exista el re-bootstrap pasivo.

---

## 4. DKMS

El DKMS es la cara visible: sirve claves a los SAEs (ETSI-014) y habla con los
otros DKMS (ETSI-020). Es el único módulo que necesita **TLS**.

**Paso 1 — certificados** (antes de arrancar):

```bash
# genera/reutiliza la CA en ./certs y emite el cert de este DKMS.
# El 2º argumento es la IP anunciable de ESTA máquina (va al SAN del cert).
.../docker/gen-certs.sh dkms-1 10.0.0.11 ./certs
```

La CA (`ca.crt`/`ca.key`) se crea la primera vez y **se reutiliza si ya
existe**: una sola CA firma todos los DKMS de la red. Multi-institución: la CA
común es un acuerdo (una CA central que firme cada cert, o CAs cross-firmadas);
se distribuye `ca.crt` a todos, la `ca.key` no sale de quien firma.

**Paso 2 — directorio y ficheros:**

```bash
mkdir dkms && cd dkms          # con ./certs dentro
cp .../docker/compose/dkms.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.dkms.yml node.yml
```

**Paso 3 — editar `node.yml`**:

```yaml
node_id: "dkms-1"                  # convención: dkms-<id de nodo>; debe
                                   # coincidir con el nombre del cert
advertise_ip: "10.0.0.11"          # IP de ESTA máquina alcanzable por los
                                   # otros DKMS (mTLS + socket ACK); debe
                                   # estar en el SAN del cert (gen-certs.sh)

orr_addr: "127.0.0.1:20003"        # su ORR (localhost si co-locado)
sdn_endpoint: "http://10.0.0.100:19000"

peers:                             # cada OTRO DKMS con el que se intercambian claves
  dkms-2:
    endpoint: "10.0.0.12"          # puerto peer 20006 por defecto
    orr_id: "orr_2"                # el ORR de ese peer

# security_level: qkd_prefer      # strict_qkd | qkd_prefer | no_worry
# fill_rate: 0                    # floor de llenado keys/s (0 = lo que asigne la SDN)
```

**Paso 4 — arrancar y verificar:**

```bash
docker compose -f dkms.yml up -d && docker compose -f dkms.yml logs -f
```

Logs sanos:

```
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20005 plane="sae"
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20006 plane="peer-dkms"
dkms::service: orr deliveries pump connected subscriber=dkms-dkms-1
dkms::control::generator: generator.state peer=dkms-2 enc=4096 dec=4096 ack_pending=0 ...
```

La línea `generator.state` (cada 5 s) es el estado real de los buffers.
Warnings `ack_reaper: expired pending keys` durante el primer minuto son un
transitorio del arranque (claves emitidas antes de que el peer estuviera
arriba); en estado estacionario `ack_pending` debe tender a 0.

---

## Sitio completo en una máquina (`site.yml`)

Si el QKC+ORR+DKMS de un nodo van en la misma máquina, un solo compose:

```bash
mkdir mi-nodo && cd mi-nodo      # con node.qkc.yml, node.orr.yml, node.dkms.yml y certs/
cp .../docker/compose/site.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
docker compose -f site.yml up -d
```

Deja `qkc_addr`/`orr_addr` en `127.0.0.1` (co-locados). Los rangos de puertos
por rol no colisionan.

## SAEs: certs y prueba de humo ETSI-014

Los SAEs (clientes que piden claves) también necesitan cert firmado por la CA,
con el SAN que el DKMS entiende (`urn:dkms:sae:<id>` — con otro formato el
`dec_keys` devuelve `key not found`):

```bash
.../docker/gen-certs.sh --sae sae_1 ./certs
.../docker/gen-certs.sh --sae sae_2 ./certs
```

Intercambio completo de una clave entre dos nodos:

```bash
C=./certs
# 1) sae_1 pide a SU dkms (nodo 1) una clave con sae_2
curl -s --cacert $C/ca.crt --cert $C/sae_1.crt --key $C/sae_1.key \
  -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
  https://10.0.0.11:20005/api/v1/keys/sae_2/enc_keys
# -> {"keys":[{"key_ID":"<uuid>","key":"<b64>"}]}

# 2) sae_2 recoge la MISMA clave en el dkms del nodo 2 con ese key_ID
curl -s --cacert $C/ca.crt --cert $C/sae_2.crt --key $C/sae_2.key \
  -H 'Content-Type: application/json' \
  -d '{"key_IDs":[{"key_ID":"<uuid>"}]}' \
  https://10.0.0.12:20005/api/v1/keys/sae_1/dec_keys
# -> la misma "key" => la red funciona end-to-end
```

También hay `GET /api/v1/keys/<slave>/status` para consultar stock sin gastar.

## Escape hatch (config avanzada)

Si prefieres el TOML nativo, monta `qkc.toml` (qkc) o `default.toml`
(+ `topology/` para sdn) en `/config` y el entrypoint lo usa tal cual, sin
generar nada.

## Troubleshooting

| síntoma | causa probable |
|---------|----------------|
| `dec_keys` → `key not found` con `enc_keys` OK | cert del SAE sin SAN `urn:dkms:sae:<id>` (usa `gen-certs.sh --sae`), o el peer DKMS de destino no está en `peers:` del emisor |
| curl → `alert certificate required` | falta `--cert/--key` del SAE, o su cert no está firmado por la CA que el DKMS tiene en `certs_dir` |
| SDN `qkcs_err>0` permanente | la IP/puerto del QKC en la topología no es alcanzable desde la SDN (firewall del 20002) |
| ORR "sin master_secret" en bucle | se reinició un solo ORR; reinicia el conjunto |
| QKC sin `handshake.established` | vecino caído o puerto 20000 filtrado entre las dos máquinas |
| `ack_pending` crece sin parar | puerto 20009 del peer filtrado, o `advertise_ip` mal puesto (los ACK van a esa IP) |
| DKMS `qkc unreachable after 20 retries; continuing without it` en el boot | **normal** en despliegues con ORR (el DKMS no habla con el QKC directamente) |

## Notas multi-institución

- **SDN central única**: es el modelo soportado hoy. Autonomía/federación por
  institución NO existe todavía.
- **Topología inmutable en runtime**: se carga en el boot de la SDN; añadir
  nodos/enlaces = editar `node.yml` y reiniciar la SDN.
- **Sin límites de RAM/CPU** en los compose (default de Docker). En máquinas
  pequeñas añade `mem_limit:` tú mismo.
