# Despliegue por contenedores (multi-institución, manual)

Imágenes Docker públicas, una por módulo (`qkc`, `orr`, `dkms`, `sdn`), para que
**cada institución despliegue lo suyo de forma autónoma**: `docker compose up`
rellenando solo un `node.yml` corto. Sin orquestador central.

- **Sin QKD real** → el QKC usa enlaces **PQC** ("QKD simulado por PQC"). No hace
  falta quditto.
- **Con QKD real** → el enlace del QKC apunta al **KME ETSI-014** del hardware.

## Requisitos
- Docker + `docker compose` en cada máquina (Raspberry Pi OS, Debian, etc.).
- Conectividad de red entre las máquinas/instituciones que deben hablarse
  (QKC↔QKC, DKMS↔DKMS, y todos → SDN). VPN/rutas/IPs públicas según tu red.

## Despliegue de un módulo (institución) — 3 pasos
```bash
mkdir mi-qkc && cd mi-qkc
cp .../docker/compose/qkc.yml .          # el compose del rol
cp .../docker/compose/.env.example .env  # pon IMAGE_PREFIX=<namespace real>
cp .../docker/examples/node.qkc.yml node.yml
# 1) editar node.yml con tus datos (ver examples/)
docker compose -f qkc.yml pull           # baja la imagen de Docker Hub
docker compose -f qkc.yml up -d          # arranca (restart automático)
docker compose -f qkc.yml logs -f        # ver estado
```
Igual para `orr`, `dkms`, `sdn` (cada uno con su `node.yml`). Para un **nodo
entero en una máquina** usa `compose/site.yml` (qkc+orr+dkms juntos).

## Qué pones en `node.yml`
Plantillas comentadas en `examples/`:
- `node.qkc.yml` — id + enlaces (PQC o QKD-real con `kme_url`).
- `node.orr.yml` — id, dónde está su QKC, la SDN, y los ORR peers.
- `node.dkms.yml` — id, IP anunciable, dónde está su ORR, la SDN, y los DKMS peers.
- `topology.example.yml` — **la SDN**: visión global (la mantiene el operador
  central). Cópiala como `node.yml` para el contenedor `sdn`.

**Co-locado vs repartido**: si el DKMS/ORR/QKC de un nodo están en la misma
máquina, deja `qkc_addr`/`orr_addr` en `127.0.0.1`. Si están en máquinas
distintas, pon la IP de cada uno. Los puertos por defecto (abajo) no colisionan.

## TLS (solo DKMS)
El mTLS ETSI-020 entre DKMS necesita certificados con una **CA común**:
```bash
./gen-certs.sh dkms-1 <IP-anunciable-de-esta-maquina> ./certs
# reutiliza ./certs/ca.crt si existe (una sola CA firma toda la red)
```
Monta `./certs` en el DKMS (el `dkms.yml` ya lo hace). **Multi-institución**: la
CA común es un acuerdo entre instituciones (una CA central que firme, o CAs que se
cross-firmen). Distribuye `ca.crt` a todos.

## Puertos por defecto (host network)
| módulo | puertos |
|--------|---------|
| qkc | 20000 (peer), 20001 (local/ORR), 20002 (admin/SDN) |
| orr | 20003 (grpc), 20004 (metrics) |
| dkms | 20005 (SAE), 20006 (peer), 20007 (grpc), 20008 (metrics), 20009 (ACK) |
| sdn | 19000 (grpc), 19002 (http), 19010 (metrics) |

Override con `ports:` en el `node.yml` solo si una máquina corre varios nodos del
mismo rol. Firewall recomendado: exponer `admin` del QKC solo a la SDN; `peer`
del QKC y `peer` del DKMS entre las instituciones que se enlazan.

## Escape hatch (config avanzada)
Si prefieres el TOML nativo, monta `qkc.toml` (qkc) o `default.toml` (+`topology/`
para sdn) en `/config` y el entrypoint lo usa tal cual, sin generar nada.

## Para el mantenedor: construir y publicar
```bash
# desde la raíz del repo, con buildx configurado para multi-arch
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
```
Construye `qkc/orr/dkms/sdn` para `linux/amd64` + `linux/arm64` (raspi) y las sube
a Docker Hub. Nota: el build compila `aws-lc-sys` (dep de rustls) que exige
**gcc-12** — la imagen base `rust:1.88-bookworm` ya lo trae. arm64 va por
emulación QEMU (lento) o runner ARM nativo.

Prueba local de una sola arch sin push:
```bash
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load qkc
```

## Notas multi-institución
- **SDN central única**: una sola SDN (en el server central) ve toda la topología
  y empuja las tablas de forwarding a los QKC. Es el modelo soportado hoy.
  Autonomía/federación por institución NO existe todavía.
- **Sin límites de RAM/CPU** en los compose (comportamiento por defecto de
  Docker). Si una máquina es pequeña, puedes añadir `mem_limit:` tú mismo.
