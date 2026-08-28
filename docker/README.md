# Despliegue por contenedores (multi-institución, manual)

Imágenes Docker públicas, una por módulo (`qkc`, `orr`, `dkms`, `sdn`), para que
**cada institución despliegue lo suyo de forma autónoma**: `docker compose up`
rellenando solo un `node.yml` corto. Sin orquestador central.

> **¿Prisa?** En [`examples/quick_start.md`](examples/quick_start.md) están
> solo los comandos, sin explicaciones. Esta guía cuenta además **qué hace
> cada paso, qué significa cada campo y cómo verificar que funciona**.

El modelo de red: una **SDN central única** (la mantiene el operador) que ve
toda la topología, y en cada nodo/institución un trío **QKC + ORR + DKMS**
(en la misma máquina o repartidos):

```
            ┌──────────── SDN (operador central) ────────────┐
            │  ve la topología, resuelve el LP de rates y    │
            │  empuja forwarding a los QKC                   │
            └──┬──────────────────┬──────────────────┬───────┘
   institución 1                  │                  │
┌──────────────────┐   ┌──────────────────┐   ┌──────────────────┐
│ DKMS-1 (claves a │   │ DKMS-2           │   │ DKMS-3           │
│  SAEs, ETSI-014) │◄──┼─►ETSI-020 mTLS◄──┼───┼─►                │
│   │              │   │   │              │   │   │              │
│ ORR-1 (transporte│◄──┼─►ORR-2 (gRPC)◄───┼───┼─►ORR-3           │
│  E2E de material)│   │   │              │   │   │              │
│   │              │   │   │              │   │   │              │
│ QKC-1 (enlaces   │◄──┼─►QKC-2 (TCP)◄────┼───┼─►QKC-3           │
│  QKD/PQC)        │   │                  │   │                  │
└──────────────────┘   └──────────────────┘   └──────────────────┘
```

- **Sin QKD real** → el QKC usa enlaces **PQC** ("QKD simulado por PQC",
  ML-KEM-768 con re-keying periódico). No hace falta quditto.
- **Con QKD real** → el enlace del QKC apunta al **KME ETSI-014** del hardware.

Flujo validado end-to-end el 2026-07-02 sobre 4 máquinas (campaña local
`proxmox-docker-smoke`): claves ETSI-014 idénticas en ambos extremos.

## Cómo funciona una imagen por dentro (léelo una vez)

Cada imagen contiene el binario Rust de su módulo + un entrypoint común:

1. El contenedor arranca con `ROLE` fijado (qkc|orr|dkms|sdn).
2. El entrypoint busca `/config/node.yml` (el que montas tú) y lo convierte
   con `render_config.py` en la config nativa del binario — un TOML — escrita
   en `/run/cfg/`. La SDN no lleva topología: la infiere de los anuncios.
3. Arranca el binario apuntando a esa config.

Consecuencias prácticas:

- **Tú solo editas `node.yml`**; nombres de campo y puertos por defecto los
  pone el renderer (espejo de los `src/config.rs` de cada crate).
- Para **depurar la config real** que recibió el binario:
  `docker compose -f <rol>.yml exec <rol> cat /run/cfg/qkc.toml` (qkc) o
  `.../run/cfg/default.toml` (resto).
- **Escape hatch**: si montas un TOML crudo (`qkc.toml` para qkc,
  `default.toml` para el resto, en `/config`), el entrypoint lo usa tal cual y
  no genera nada. Útil para configs que el node.yml no expone.

Los compose de `compose/` son deliberadamente mínimos: `network_mode: host`
(sin mapeo de puertos: el binario escucha directamente en la máquina),
`restart: unless-stopped` (rearranque automático tras reinicio o crash) y el
`node.yml` montado read-only.

## Requisitos

- Docker + `docker compose` en cada máquina (Debian, Raspberry Pi OS, etc.):
  `curl -fsSL https://get.docker.com | sudo sh`
- Conectividad entre las máquinas que deben hablarse (tabla siguiente).
  Entre instituciones: IPs públicas, VPN (WireGuard) o rutas acordadas.
- Para el DKMS: certificados firmados por una **CA común** a toda la red
  (sección TLS del paso 4).

## Puertos: quién se conecta a quién

Con `network_mode: host` los puertos se abren directamente en la máquina.
La columna "quién entra" es la que importa para el firewall:

| módulo | puerto | protocolo | quién entra |
|--------|--------|-----------|-------------|
| qkc  | 20000 (peer)   | TCP binario | los QKC vecinos (ambos sentidos) |
| qkc  | 20001 (local)  | TCP binario | **su** ORR (localhost si co-locados) |
| qkc  | 20002 (admin)  | HTTP | **solo la SDN** (push de forwarding) |
| orr  | 20003 (grpc)   | gRPC | los ORR peers y **su** DKMS |
| orr  | 20004 (metrics)| HTTP | Prometheus (opcional) |
| dkms | 20005 (sae)    | HTTPS mTLS | los SAEs (ETSI-014) |
| dkms | 20006 (peer)   | HTTPS mTLS | los otros DKMS (ETSI-020) |
| dkms | 20007 (grpc)   | gRPC | interno del nodo |
| dkms | 20008 (metrics)| HTTP | Prometheus (opcional) |
| dkms | 20009 (ack)    | TCP plano | los otros DKMS (ACKs del generator) |
| sdn  | 19000 (grpc)   | gRPC | todos los ORR y DKMS |
| sdn  | 19002 (http)   | HTTP | admin |
| sdn  | 19010 (metrics)| HTTP | Prometheus (opcional) |
| quditto | 20010 (http)| HTTP ETSI-014 | los QKC de **los dos extremos** del enlace |

Reglas mínimas entre instituciones que se enlazan: 20000, 20003, 20006 y
20009 entre ellas; 20002 solo desde la SDN; 19000 abierto hacia la SDN desde
todas. Los `metrics` y el 19002 pueden quedarse cerrados.

Override de puertos: bloque `ports:` en el `node.yml` del módulo — solo
necesario si una máquina corre **varios nodos del mismo rol** (p. ej. tests).

### Seguridad y firewall (léelo — modelo completo en `docs/SECURITY.md`)

La autenticación fuerte está **solo** donde debe: mTLS DKMS↔SAE (siempre) y,
opt-in, el plano de control (SDN, announce, gRPC) y el handshake PQC QKC↔QKC.
El resto **depende de que estos puertos vivan en red confiable**:

- **`grpc` del DKMS (20007)**: plano de operador SIN auth; su RPC `Drain` borra
  todos los buffers de una llamada. Bindea a **localhost por defecto**; si lo
  abres, firewaléalo a la red interna. Nunca entre instituciones.
- **`ack` del DKMS (20009)**: TCP plano sin auth hoy (migración a ETSI-020
  pendiente). Ábrelo solo entre los DKMS que se enlazan.
- **`local`/`admin` del QKC, `grpc` del ORR hacia su DKMS**: intra-institución.
  DKMS↔ORR lleva material de clave en claro en ese salto: **deben compartir
  host o red L2 confiable**.
- **`metrics` (todos)**: sin auth y responden a cualquier path — red interna.
- **SDN `http`/`grpc` (19000/19002)**: en multi-host, actívales mTLS
  (`control_tls: true` en su `node.yml` + certs de `net-ca`) — si no, cualquiera
  con acceso de red puede registrar nodos o rebindear SAEs.

## Paso 0 (mantenedor): construir y publicar las imágenes

Lo hace **una sola persona, una vez por versión** — las instituciones solo
hacen `pull`.

```bash
# desde la raíz del repo, con buildx configurado para multi-arch
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
```

Qué hace: compila los 5 binarios **en una sola pasada** (la etapa de build es
común a los 5 targets) y publica `tuusuario/{qkc,orr,dkms,sdn,quditto}:latest`
para `linux/amd64` + `linux/arm64` (Raspberry Pi). Variables del bake:
`IMAGE_PREFIX` (namespace en Docker Hub) y `TAG` (default `latest`).

`quditto` solo se despliega si quieres enlaces QKD **sin hardware** (ver la
sección del simulador más abajo); los otros cuatro son los módulos de runtime.

Notas de build:

- `aws-lc-sys` (dependencia de rustls) exige **gcc-12**; la base
  `rust:1.88-bookworm` ya lo trae. El toolchain lo fija `rust-toolchain.toml`
  (1.88) — si el build falla con "rustc X is not supported", el pin y el
  `Cargo.lock` se han desalineado.
- arm64 sin runner ARM va por emulación QEMU (lento pero funciona).

Variantes:

```bash
# probar en local una sola arch, sin push (deja las imágenes en el docker local)
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' --load

# lab sin registry: mover imágenes por SSH
docker save tuusuario/qkc:latest | ssh otra-maquina docker load
```

## Estructura común de un despliegue

Cada módulo se lanza igual: un directorio con 3 ficheros (+ `certs/` si es
DKMS).

```
mi-modulo/
├── <rol>.yml      # compose del rol (cópialo de docker/compose/, no se edita)
├── .env           # IMAGE_PREFIX=<namespace>  (lo lee docker compose)
└── node.yml       # LO ÚNICO que se edita (plantillas en docker/examples/)
```

Comandos idénticos para los 4 roles:

```bash
docker compose -f <rol>.yml pull      # baja/actualiza la imagen
docker compose -f <rol>.yml up -d     # arranca en segundo plano
docker compose -f <rol>.yml logs -f   # sigue los logs (Ctrl-C no para el servicio)
docker compose -f <rol>.yml restart   # reinicio (relee node.yml)
docker compose -f <rol>.yml down      # parar y quitar
```

**Orden de arranque recomendado**: SDN primero y luego el resto en cualquier
orden (QKC → ORR → DKMS da los logs más limpios). No es crítico porque todo
reintenta: el DKMS reintenta la SDN 30×1 s en el boot, los ORR
re-bootstrapean a sus peers, y la SDN re-empuja el forwarding en cada tick
hasta que todos los QKC respondan. Los warnings de los primeros ~60 s son
transitorios de este baile; lo que importa es el estado estacionario.

En los ejemplos: SDN en `10.0.0.100`, nodo 1 en `10.0.0.11`, nodo 2 en
`10.0.0.12`. Sustituye por tus IPs.

---

## 1. SDN (operador central)

**Qué es**: el plano de control. **Infiere** la topología global de lo que los
módulos le cuentan al arrancar, resuelve el reparto de tasas (LP MCMCF-λ) y
empuja a cada QKC su tabla de forwarding.

No hay coordinación fuera de banda: una institución no le comunica sus IPs al
operador para que las meta a mano. Despliega sus módulos apuntando a esta SDN
y aparecen en el grafo; si los apaga, desaparecen.

**Paso 1 — directorio y ficheros:**

```bash
mkdir sdn && cd sdn
cp .../docker/compose/sdn.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.sdn.yml node.yml
```

**Paso 2 — editar `node.yml`: casi nada.** La SDN **no lleva topología**.
Arranca con el grafo vacío y lo construye con lo que cada módulo le cuenta al
registrarse. Su `node.yml` solo tiene lo suyo, y todo con defaults sensatos:

```yaml
# mcf_period_ms: 5000
# presence_ttl_secs: 90
```

| campo | significado |
|-------|-------------|
| `listen_ip` / `ports` | dónde escucha (19000 gRPC, 19002 HTTP, 19010 métricas). Override solo si chocan. |
| `mcf_period_ms` | latido del recompute del LP (default 5000). Los cambios de topología lo disparan igualmente, sin esperar al tick. |
| `presence_ttl_secs` | un módulo que deja de anunciarse más de este tiempo sale de la topología, con sus enlaces. Debe superar el `sdn_announce_secs` de los módulos (30 por defecto): son 3 anuncios perdidos. `0` lo desactiva. |

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f sdn.yml up -d && docker compose -f sdn.yml logs -f
curl -s http://localhost:19002/topology     # al principio: todo a 0
```

Según arrancan los módulos, el grafo se va llenando solo:

```bash
curl -s http://localhost:19002/topology     # {"qkcs":2,"orrs":2,"dkms":2,"edges":1,...}
curl -s http://localhost:19002/qkcs
curl -s http://localhost:19002/links
```

Logs sanos y qué significan:

```
sdn::http_api: qkc registered qkc=1 added=["2"] pending=[]
    → un QKC se dio de alta; `pending` lista vecinos que aún no han arrancado
sdn::service: forwarding push done ... qkcs_ok=2 qkcs_err=0
    → la SDN alcanzó el puerto admin (20002) de los 2 QKC y les empujó forwarding
sdn::service: MCMCF-λ recomputed n_commodities=2 n_edges=1 lambda=... flows_with_rate=2
    → el LP corre; n_commodities = pares DKMS×2 sentidos
```

`qkcs_err>0` mientras los QKC no están arriba es normal (se recupera solo).
Si se queda permanente, el 20002 está filtrado desde la SDN o el
`advertise_ip` del QKC es incorrecto.

**Operación**: **no hay alta manual**. Una institución nueva despliega sus
módulos con el `sdn_url` de esta SDN y aparece sola; si los apaga, desaparece
sola al pasar el `presence_ttl_secs`. La SDN no se reinicia nunca por un
cambio de topología.

Si un módulo no aparece, mira su log: dirá `anunciado a la SDN` con
`accepted`/`pending`, o el error de por qué no puede.

---

## 2. QKC

**Qué es**: la capa de material de clave del nodo. Mantiene un enlace con
cada QKC vecino — por hardware QKD real (habla ETSI-014 con el KME) o por PQC
(deriva material con ML-KEM y lo renueva periódicamente) — y llena con él sus
keystores por peer. No se le configura enrutado: **la tabla de forwarding se
la empuja la SDN** al puerto admin.

**Paso 1 — directorio y ficheros:**

```bash
mkdir qkc && cd qkc
cp .../docker/compose/qkc.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.qkc.yml node.yml
```

**Paso 2 — editar `node.yml`:**

```yaml
qkc_id: 1

sdn_url: "10.0.0.100"          # la SDN se entera sola de este nodo
advertise_ip: "10.0.0.11"      # IP por la que la SDN alcanza a ESTE QKC

links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"       # IP (o IP:puerto) del QKC vecino
    type: pqc
  # con nodo QKD real:
  # - neighbor_id: 3
  #   neighbor_addr: "10.0.0.13"
  #   type: qkd
  #   kme_url: "https://mi-kme:443"
  #   r0: 2000                       # modelo del enlace, para el solver de la SDN
  #   alpha: 0.2
  #   distance_km: 5
```

| campo | significado |
|-------|-------------|
| `qkc_id` | id numérico del nodo. |
| `sdn_url` | HTTP admin de la SDN (puerto 19002 si no se indica). El QKC se anuncia solo y la SDN lo mete en su topología. Omítelo si prefieres dar de alta el nodo a mano. |
| `advertise_ip` | IP con la que se anuncia. Hace falta porque el contenedor bindea `0.0.0.0`, que no le sirve a la SDN para llamarle de vuelta. |
| `sdn_announce_secs` | cada cuánto reanuncia (default 30). Es también su heartbeat. |
| `key_size_bits` | tamaño de las claves OTP del keystore (default 256). **Debe coincidir en los dos extremos de cada enlace.** |
| `links[].neighbor_id` | id del QKC vecino. Es lo único de topología que un QKC declara, y basta con que lo haga **uno** de los dos extremos: la SDN monta la arista y se lo cuenta al otro. Lo declarado aquí es además un suelo que la SDN no puede quitar. |
| `links[].neighbor_addr` | **opcional** en enlaces `pqc` si hay `sdn_url`: la dirección no viaja en el anuncio —la SDN ya la conoce, porque cada QKC anuncia la suya— y vuelve en la lista de peers. Omítela y el enlace se monta cuando la SDN conteste; declárala y se monta en el arranque, sin depender de ella. En un enlace `qkd` es obligatoria (la SDN no los crea). Puerto peer 20000 si no se indica. |
| `links[].type` | `pqc` (sin hardware) o `qkd` (con `kme_url` del KME ETSI-014). **Un enlace `qkd` hay que declararlo sí o sí**: la SDN no puede inventarse el `kme_url` de tu institución, así que si ofrece uno sin config local el QKC lo avisa por log y no lo crea. |
| `links[].r0` / `alpha` / `distance_km` | modelo físico del enlace `qkd`. El QKC no los usa: se los pasa a la SDN, que dimensiona la arista con `r0·10^(−alpha·d/10)`. |
| `links[].capacity_keys_per_s` | capacidad declarada de un enlace `pqc` en claves/s (ignorada en `qkd`). Sin declarar, la SDN aplica 10 000 — un default finito con el que la señal de rates significa algo también en despliegues solo-PQC (antes llevaban un centinela de 1e9 y `/rate` era ruido). |
| `links[].pqc_*` | solo PQC, opcionales: `pqc_suite` (default `ml-kem-768`), `pqc_rekey_keys` (rota el secreto cada N claves, default 1000), `pqc_rekey_secs` (…o cada T segundos, default 3600), `pqc_rekey_lookahead` (épocas pre-derivadas, default 2). |
| `links[].link_psk` | **secreto pre-compartido del enlace**, base64 de 32 bytes, IDÉNTICO en los dos extremos. Es la raíz de la autenticación del enlace: con él se firma el handshake, el NOTIFY y —si `frame_auth` lo pide— cada frame de datos. Solo config local: la SDN no transporta secretos, ni debe. Sin él no hay autenticación de enlace ninguna. |
| `links[].frame_auth` | `off` (default) \| `prefer` \| `require`: MAC por frame de datos. Ver la sección de abajo. |

**Sobre el auto-registro.** Una arista necesita a sus dos extremos dados de
alta, así que el QKC que arranque primero la verá `pending` hasta que su vecino
aparezca: el anuncio es un bucle, no un disparo único, y converge solo. Un
reanuncio sin cambios no toca la topología, así que no dispara recálculos.

Si los dos extremos declaran `r0`/`alpha`/`distance_km` **distintos**, la SDN se
queda con el primero que llegó y lo avisa por log; corrige el `node.yml` que
esté mal, porque de ese número sale la capacidad del enlace.

### Autenticar el enlace (`link_psk` + `frame_auth`)

El payload del enlace va cifrado con OTP, que da confidencialidad y **cero
integridad**: XOR es maleable, así que quien pueda tocar el cable puede
aplicarle un delta al ciphertext y el receptor descifra un mensaje modificado
sin enterarse. Y `sender_id` viaja en claro sin comprobar. Esto lo cierra un
HMAC-SHA256 por frame, con contador y ventana anti-replay
(`docs/SECURITY.md` §Fase 8).

**Hace falta una `link_psk` por enlace**, de 32 bytes, idéntica en los dos
extremos y repartida fuera de banda — es la misma suposición que hace el propio
QKD para su canal clásico autenticado. Es simétrica y de 256 bits, así que es
quantum-safe (Grover deja 128 efectivos):

```bash
openssl rand -base64 32     # una por ENLACE, no por nodo
```

```yaml
links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"
    type: pqc
    link_psk: "aJUINhXK1WJhWEoelgeo9ZrtCNQv8xQGLPJdMhNWV6g="   # la MISMA en el vecino
    frame_auth: require
```

Tres cosas que hay que saber antes de activarlo:

1. **Un enlace con `frame_auth` hay que declararlo en los DOS extremos**, con la
   misma PSK. La raíz es config local y la SDN no la reparte, así que el extremo
   que aprende el enlace por anuncio se queda sin ella y **descarta todo lo que
   le llega** de ese vecino. Es el mismo caso que `pqc_auth: sign`.
2. **Despliega en `prefer` antes de subir a `require`.** Un peer que aún no
   entienda los frames autenticados los ignora en silencio, así que pasar
   directamente a `require` con un extremo sin actualizar deja el enlace muerto.
   El orden seguro es: reparte PSKs → `prefer` en todos → comprueba → `require`.
3. **`require` sin `link_psk` no arranca**, a propósito: correr sin autenticar
   creyendo que sí es justo lo que el flag existe para impedir.

Con la PSK puesta, el NOTIFY se autentica siempre —incluso en `frame_auth: off`—
porque es el plano de control del enlace: decide qué `key_ID` le pide este QKC a
su KME, y eso el material QKD no lo protege.

Para verlo funcionando, cada 5 s y por enlace:

```
qkc::service: qkc.frame_auth me=1 peer=2 mode=Require signed=544746 verified=428948
              bad_mac=0 replayed=0 plain_ok=0 plain_rej=0
```

`bad_mac`, `replayed` y `plain_rej` deben quedarse en 0. `plain_ok` subiendo con
`prefer` significa que el otro extremo todavía no firma — es lo normal a mitad
del despliegue, y lo que tiene que llegar a 0 antes de subir a `require`.
`plain_rej` subiendo con `require` es config asimétrica: a alguien le falta la
PSK. Coste medido en CESGA (n=10, enlaces QKD, régimen medio): −0,04 %.

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f qkc.yml up -d && docker compose -f qkc.yml logs -f
```

```
qkc::pqc_handshake: qkc.pqc.handshake.established me=1 peer=2 epoch=N
    → enlace PQC vivo con el vecino (una línea por vecino)
qkc::keystore: keystore.levels peer=2 enc=256 dec=256 taken=4096 misses=0
    → keystores por peer llenándose/rotando; misses=0 = nadie pidió material
      que no hubiera
```

Sin `handshake.established`: el vecino está caído, su `node.yml` no declara
este enlace, o el 20000 está filtrado entre ambas máquinas.

---

### Cifrar el gRPC DKMS↔ORR y ORR↔ORR (`grpc_tls`)

Ese gRPC va **en claro** por defecto, y por él pasa el material de transporte
sin cifrar: está bien mientras DKMS y ORR compartan máquina o una red interna
de confianza, y no en otro caso. Para cifrarlo con mTLS, con los mismos
certificados de nodo de la CA de red:

```yaml
# node.orr.yml
control_tls: true          # identidad del ORR: certs/<orr_id>.crt/.key + net-ca.crt
grpc_tls: true             # el gRPC exige cert de cliente de la CA de red
# node.dkms.yml
orr_tls: true              # dial https:// al ORR
```

Es un ajuste de despliegue: en los **dos** extremos, y en **todos** los ORR a la
vez, porque las direcciones de los pares que reparte la SDN llegan como
`http://` y cada ORR les cambia el esquema según su propio `grpc_tls`. Hace
falta un certificado de nodo por ORR (`gen-certs.sh <orr_id> <ip>`). El ORR
lo dice al arrancar: `orr gRPC listening (mTLS)`.

## 3. ORR

**Qué es**: el transporte E2E de material entre nodos. Coge claves del QKC de
su nodo, las envuelve (cebolla, PQC E2E con el ORR destino) y las entrega al
DKMS remoto vía su ORR. Habla con: **su** QKC (20001), la SDN (19000) y los
ORR de los demás nodos (20003).

**Paso 1 — directorio y ficheros:**

```bash
mkdir orr && cd orr
cp .../docker/compose/orr.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.orr.yml node.yml
```

**Paso 2 — editar `node.yml`:**

```yaml
orr_id: "orr_1"
qkc_id: 1
qkc_addr: "127.0.0.1:20001"
sdn_url: "http://10.0.0.100:19000"

peers:
  orr_2: 2
peer_grpc_addrs:
  orr_2: "http://10.0.0.12:20003"
```

| campo | significado |
|-------|-------------|
| `orr_id` | **convención obligada**: `orr_<id de nodo>` — la SDN y los DKMS lo derivan así. |
| `qkc_id` | el nodo al que pertenece. |
| `qkc_addr` | dónde está **su** QKC (puerto local 20001). `127.0.0.1` si co-locados; la IP del QKC si va en otra máquina. |
| `sdn_url` | gRPC de la SDN central. |
| `advertise_ip` | IP por la que la SDN alcanza a este ORR. Ponla y el ORR se da de alta solo en la topología; sin ella hay que darlo de alta a mano, y el ORR lo dice por log al arrancar (`no sé con qué IP anunciarme`). |
| `sdn_announce_secs` | cada cuánto reanuncia (default 30). Es también su heartbeat. |
| `peers` / `peer_grpc_addrs` | **semilla, opcional**: los ORR con los que arrancar el bootstrap antes de que la SDN conteste. La lista viva la manda la SDN en la respuesta al anuncio, y un ORR nuevo aparece solo. Lo que pongas aquí es además un suelo que la SDN no puede borrar. Ojo a que no son solo los vecinos físicos: el bootstrap PQC ORR↔ORR es extremo a extremo e independiente de la topología de enlaces. `peers` mapea `orr_id → qkc_id`; `peer_grpc_addrs` mapea `orr_id → URL` (20003). |
| `default_max_hops` | déjalo en 1 (PQC E2E, el modo que usa el DKMS). |

**Paso 3 — arrancar y verificar:**

```bash
docker compose -f orr.yml up -d && docker compose -f orr.yml logs -f
```

```
orr::bootstrap: orr.peer_pubkey bootstrap ok local=orr_1 peer=orr_2 suite=ml-kem-768
orr::bootstrap: orr.bootstrap bootstrap_secret ok local=orr_1 peer=orr_2
    → secreto maestro PQC establecido con ese peer (par de líneas por peer)
orr::grpc_server: orr.stream_deliveries subscribed subscriber=dkms-dkms-1
    → su DKMS se ha conectado y escucha entregas
```

**Gotcha conocido**: si reinicias **solo un** ORR, los peers conservan el
`master_secret` viejo y el nuevo loguea "sin master_secret" en bucle. Hoy no
hay re-bootstrap pasivo: reinicia también los ORR peers (o el conjunto).

---

## 4. DKMS

**Qué es**: la cara visible del nodo. Sirve claves a los SAEs por ETSI-014
(mTLS, 20005), acuerda claves con los otros DKMS por ETSI-020 (mTLS, 20006) y
mantiene en RAM buffers de claves de transporte por peer que un *generator*
rellena en segundo plano a la tasa que dicta la SDN (los ACKs de ese flujo van
por el 20009). Es el único módulo con TLS, así que tiene un paso extra.

**Paso 1 — certificados.** Dos planos mTLS, misma CA:

- **Plano SAE (20005)**: el DKMS presenta su cert de servidor; el SAE presenta
  un cert de cliente del que el DKMS **extrae su identidad** (SAN
  `urn:dkms:sae:<id>`, o CN/DNS con el id pelado).
- **Plano peer (20006)**: mTLS entre DKMS; ambos validan contra la CA común.

```bash
# genera/reutiliza la CA en ./certs y emite el cert de ESTE dkms.
# El 2º argumento es la IP anunciable de esta máquina: va al SAN del cert
# (los peers la verifican) y DEBE ser la misma que advertise_ip del node.yml.
.../docker/gen-certs.sh dkms-1 10.0.0.11 ./certs
```

Produce **dos raíces** `net-ca.crt`/`net-ca.key` (nodos) y
`sae-ca.crt`/`sae-ca.key` (SAEs) — solo la primera vez, después **reutiliza**
las que encuentre — y `dkms-1.crt`/`dkms-1.key` (firmado por net-ca) con SAN
`URI:dkms://dkms-1, IP:10.0.0.11, DNS:localhost`. Ver `docs/SECURITY.md` §2.

**Multi-institución**: `net-ca` es la raíz COMÚN de la federación (el mTLS
entre DKMS la exige); se distribuye `net-ca.crt` a todos y **`net-ca.key` no
sale de quien firma**. `sae-ca` puede ser por institución (cada DKMS pone en
`sae_client_ca` la CA de SUS SAEs). Separarlas impide que un cert de SAE valga
como cert de DKMS. El nombre del fichero de cert de nodo debe ser exactamente
`<node_id>.crt`/`.key` — el binario los busca por ese nombre en `/config/certs`.

**Paso 2 — directorio y ficheros:**

```bash
mkdir dkms && cd dkms            # con ./certs dentro
cp .../docker/compose/dkms.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
cp .../docker/examples/node.dkms.yml node.yml
```

**Paso 3 — editar `node.yml`:**

```yaml
node_id: "dkms-1"
advertise_ip: "10.0.0.11"
orr_addr: "127.0.0.1:20003"
sdn_endpoint: "http://10.0.0.100:19000"

peers:
  dkms-2:
    endpoint: "10.0.0.12"
    orr_id: "orr_2"
```

| campo | significado |
|-------|-------------|
| `node_id` | **convención obligada**: `dkms-<id de nodo>`. Debe coincidir con el nombre del cert. |
| `advertise_ip` | IP de esta máquina **alcanzable por los otros DKMS**: se anuncia como endpoint de ACK (20009) y debe estar en el SAN del cert. Si está mal, los ACKs del generator no vuelven y `ack_pending` crece sin parar. |
| `orr_addr` | su ORR (20003). `127.0.0.1` si co-locados. |
| `sdn_endpoint` | gRPC de la SDN. En el boot se reintenta 30×1 s; si la SDN aparece más tarde, reinicia el DKMS. |
| `orr_id` | id del ORR del que cuelga este DKMS. La SDN lo coloca en el grafo por él, y `orr_addr` no vale: es una dirección, no un id. |
| `sdn_announce_secs` | cada cuánto reanuncia (default 30). Es también su heartbeat. |
| `peers.<id>` | **semilla, opcional**: con qué DKMS trabajar mientras la SDN no conteste, y suelo que la SDN no puede borrar. `endpoint` (IP, puerto peer 20006 por defecto) y `orr_id` (el ORR de ese peer, por el que viaja el material). Cuando la SDN responde manda ella el `endpoint` y el `orr_id`; `max_hops`, `security_level` y `sni` se quedan siempre en local. |
| `security_level` | default para servir claves: `strict_qkd` (solo material grado QKD; falla si no hay), `qkd_prefer` (default: QKD si hay, si no PQC), `no_worry` (lo que haya). El SAE puede pedir un nivel distinto por request; esto es el default. |
| `fill_rate` | suelo de llenado del generator en keys/s (default 0 = solo lo que asigne la SDN). |
| `sae_bindings` | mapeo local `sae→dkms` de respaldo si la SDN no responde. Opcional. |
| `certs_dir` | default `/config/certs` (donde el compose monta `./certs`). |

**Paso 4 — arrancar y verificar:**

```bash
docker compose -f dkms.yml up -d && docker compose -f dkms.yml logs -f
```

```
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20005 plane="sae"
dkms::etsi_http::mtls: dkms https listening addr=0.0.0.0:20006 plane="peer-dkms"
    → los dos planos mTLS arriba
dkms::service: orr deliveries pump connected subscriber=dkms-dkms-1
    → conectado a su ORR y suscrito a entregas
dkms::control::generator: generator.state peer=dkms-2 enc=4096 dec=4096
                          ack_pending=0 emit_total=... observed_keys_per_s=...
                          sdn_rate_keys_per_s=...
    → la línea de salud (cada 5 s, una por peer): enc/dec = llenado de los
      buffers de claves de transporte; ack_pending → 0 en estacionario;
      sdn_rate = tasa que la SDN asigna a ese flujo
```

Dos mensajes del boot que **no** son errores: `qkc unreachable after 20
retries; continuing without it` (el DKMS no habla con el QKC directamente
cuando el transporte es ORR) y `ack_reaper: expired pending keys` durante el
primer minuto (claves emitidas antes de que el peer estuviera arriba).

---

## Enlaces QKD sin hardware: el simulador `quditto`

**Cuándo lo necesitas**: solo si quieres enlaces `type: qkd` y no tienes un KME
real. Si te vale con `type: pqc` no despliegues nada de esto — los enlaces PQC
no hablan con ninguna API, los dos QKC vecinos derivan el material entre ellos
con ML-KEM.

**Qué es**: un KME de mentira. Mantiene un buffer de claves aleatorias que
rellena a

```
R(d) = r0 · 10^(−alpha·d/10)   claves/s
```

y las sirve por ETSI-014, igual que haría el hardware. Para el QKC es
indistinguible de un KME real: apunta su `kme_url` aquí y ya está.

**Un quditto por enlace, no por nodo.** Los QKC de los dos extremos apuntan al
**mismo** `kme_url`; de ahí sale que ambos obtengan el mismo material. Lo
levanta una de las dos instituciones (o el operador) en una máquina que ambas
alcancen.

```bash
mkdir quditto-1-2 && cd quditto-1-2
cp .../docker/compose/quditto.yml .
cp .../docker/examples/node.quditto.yml node.yml   # editar r0/alpha/distance_km
echo "IMAGE_PREFIX=tuusuario" > .env
docker compose -f quditto.yml up -d
docker compose -f quditto.yml logs -f   # sano: "minter started ... rate_kps=..."
```

Comprobación:

```bash
curl -s http://localhost:20010/api/v1/keys/1/status
# {"source_KME_ID":"quditto", ..., "stored_key_count":8192, "key_size":256}
```

En el `node.yml` de **los dos** QKC del enlace basta con apuntar al simulador:

```yaml
links:
  - neighbor_id: 2
    neighbor_addr: "10.0.0.12"
    type: qkd
    kme_url: "http://10.0.0.50:20010"
    r0: 2000          # los mismos que el node.yml del quditto
    alpha: 0.2
    distance_km: 5
```

**Los tres valores van también en los QKC.** `r0`, `alpha` y `distance_km` se
declaran aquí (el quditto los usa para generar a esa tasa) y en el bloque
`links` del `node.yml` de **los dos QKC** del enlace, que se los comunican a la
SDN para dimensionar la arista en su solver. Si divergen, la SDN reparte caudal
sobre una capacidad que el enlace no da. La SDN avisa por log si los dos
extremos no coinciden entre sí, y se queda con el primero que llegó.

`key_size_bits` debe coincidir en el quditto y en los dos QKC (256 por defecto).

Escape hatch: si no montas `node.yml`, el contenedor arranca con las variables
`QUDITTO_R0`, `QUDITTO_ALPHA`, `QUDITTO_DISTANCE`, `QUDITTO_MAX_BUFFER`,
`QUDITTO_KEY_SIZE_BITS` y `QUDITTO_LISTEN` del entorno.

---

## Sitio completo en una máquina (`site.yml`)

El caso común — el trío entero del nodo en una sola máquina, un solo compose:

```bash
mkdir mi-nodo && cd mi-nodo
cp .../docker/compose/site.yml .
echo "IMAGE_PREFIX=tuusuario" > .env
# node.qkc.yml + node.orr.yml + node.dkms.yml (como en las secciones 2-4) y certs/
docker compose -f site.yml up -d
```

Deja `qkc_addr`/`orr_addr` en `127.0.0.1` (co-locados). Los rangos de puertos
por rol no colisionan entre sí.

## SAEs: certificados y prueba de humo ETSI-014

Un SAE es cualquier aplicación cliente que pide claves a su DKMS. Necesita un
cert de cliente firmado por la CA común **con la identidad en el SAN**:

```bash
.../docker/gen-certs.sh --sae sae_1 ./certs     # SAN URI:urn:dkms:sae:sae_1
.../docker/gen-certs.sh --sae sae_2 ./certs
```

> Con otro formato de SAN el DKMS no extrae la identidad: el `enc_keys`
> responde 200 pero el `dec_keys` del otro extremo da `key not found`.

API en `https://<dkms>:20005` (ETSI GS QKD 014):

| endpoint | qué hace |
|----------|----------|
| `GET /api/v1/keys/<slave>/status` | stock y límites del par: `stored_key_count`, `max_key_per_request` (64), `max_key_size` (4096), `min_key_size` (64)… |
| `POST /api/v1/keys/<slave>/enc_keys` | body `{"number":N,"size":bits}` → `{"keys":[{"key_ID","key"}]}`. El DKMS entrega la clave al DKMS del slave por ETSI-020 en la misma llamada. También `GET …/enc_keys?number=N&size=bits` (§6.2 de la spec; defaults 1/256) — es lo que usa strongSwan. |
| `POST /api/v1/keys/<master>/dec_keys` | body `{"key_IDs":[{"key_ID":"…"}]}` → la **misma** clave, en el otro extremo. También `GET …/dec_keys?key_ID=<uuid>` (§6.4: una sola key por GET). |

Intercambio completo entre dos nodos (la prueba de que la red funciona):

```bash
C=./certs
# 1) sae_1 pide a SU dkms (nodo 1) una clave con sae_2
curl -s --cacert $C/net-ca.crt --cert $C/sae_1.crt --key $C/sae_1.key \
  -H 'Content-Type: application/json' -d '{"number":1,"size":256}' \
  https://10.0.0.11:20005/api/v1/keys/sae_2/enc_keys
# -> {"keys":[{"key_ID":"<uuid>","key":"<b64>"}]}

# 2) sae_2 recoge esa clave en el dkms del nodo 2
curl -s --cacert $C/net-ca.crt --cert $C/sae_2.crt --key $C/sae_2.key \
  -H 'Content-Type: application/json' -d '{"key_IDs":[{"key_ID":"<uuid>"}]}' \
  https://10.0.0.12:20005/api/v1/keys/sae_1/dec_keys
# -> la misma "key" => end-to-end OK
```

## Operación día a día

```bash
# actualizar un módulo a la última imagen publicada
docker compose -f <rol>.yml pull && docker compose -f <rol>.yml up -d

# cambiar la config: editar node.yml y
docker compose -f <rol>.yml restart

# ver la config renderizada que recibió el binario
docker compose -f <rol>.yml exec <rol> cat /run/cfg/default.toml   # (qkc: /run/cfg/qkc.toml)
```

### Añadir una institución nueva

**No hay que tocar los nodos que ya corren.** Despliegas el trío nuevo con su
`node.yml`, se anuncia a la SDN, y los demás se enteran en su siguiente
heartbeat (≤ `sdn_announce_secs`, 30 s por defecto): la respuesta al anuncio
lleva la lista de peers que a cada módulo le tocan, derivada del grafo. El
DKMS registra al nuevo par, el ORR arranca su bootstrap PQC, y el QKC crea el
enlace en caliente.

Dos cosas siguen siendo manuales:

- **Los enlaces QKD.** Un enlace `qkd` necesita el `kme_url` del KME de esa
  institución, que la SDN no puede conocer ni inventar, así que se declara en
  el `node.yml` de los dos extremos. Si la SDN ofrece un enlace QKD para el
  que no hay config local, el QKC lo avisa por log y no lo crea. Los enlaces
  `pqc` sí se crean solos.
- **Los certificados.** El DKMS nuevo necesita un cert firmado por la CA común,
  y su `advertise_ip` en el SAN.

**`node.yml` es un suelo, no una foto.** Los peers/enlaces declarados en local
no los quita nadie: la SDN puede añadir peers y quitar los que ella añadió,
pero un peer local se queda. Sin esa regla, cada arranque en el que la SDN va
por detrás costaría enlaces y material de claves vivos — se vio en el
laboratorio, un QKC destruyendo los dos enlaces de su propio `node.yml`
segundos después de arrancar para reconstruirlos 30 s más tarde.

Al revés, dar de baja una institución sí es automático: deja de anunciarse,
`presence_ttl_secs` la expira del grafo (90 s por defecto = 3 anuncios
perdidos) y los demás la sueltan en el siguiente heartbeat. Tirar un enlace
libera su `SecretStore`, así que su material se borra.

## Troubleshooting

| síntoma | causa probable |
|---------|----------------|
| `dec_keys` → `key not found` con `enc_keys` OK | cert del SAE sin SAN `urn:dkms:sae:<id>` (usa `gen-certs.sh --sae`), o el DKMS destino todavía no conoce al emisor (mira si la SDN los tiene a los dos en el grafo) |
| curl → `alert certificate required` | falta `--cert/--key` del SAE, o su cert no lo firmó la CA que el DKMS monta en `certs_dir` |
| SDN `qkcs_err>0` permanente | `advertise_ip` del QKC mal, o 20002 filtrado desde la SDN |
| ORR "sin master_secret" en bucle | se reinició un solo ORR; reinicia el conjunto |
| QKC sin `handshake.established` | vecino caído, enlace no declarado en el otro extremo, o 20000 filtrado |
| enlace PQC vivo pero sin claves, `keystore.levels … enc=0 dec=0 taken=0` + `timeout waiting pqc-secret` | reiniciaste un extremo y la re-negociación no disparó. El extremo de id **menor** debe loguear `qkc.pqc.relink` en cuanto el otro reconecta; si no aparece, el socket saliente no se cayó (¿NAT/proxy que lo mantiene abierto?). Reiniciar el extremo menor lo resuelve |
| DKMS `ack_pending` crece sin parar | 20009 del peer filtrado o `advertise_ip` mal (los ACK van a esa IP). Lee `generator.diag` — dice cuál de los dos es |
| DKMS `qkc unreachable … continuing without it` en el boot | **normal** con transporte ORR |
| `enc_keys` lento o falla hacia un peer | el ORR local aún no tiene ese peer: o la SDN no se lo ha mandado (¿los dos anunciándose?), o su bootstrap PQC no ha terminado — busca `orr.peer_pubkey bootstrap ok` para él |
| la config no coincide con lo que esperabas | mira lo renderizado: `exec <rol> cat /run/cfg/…` |

### Diagnosticar el ciclo de claves DKMS↔DKMS

El camino tiene cuatro saltos —emito → viaja por ORR/QKC → el peer la guarda
→ su ACK vuelve por TCP 20009— y sólo cierra el ciclo si los cuatro funcionan.
Cada DKMS vuelca una línea `generator.state` por peer cada 5 s con un contador
por salto:

```
peer=dkms-2 enc=… emitted=… recv=… ack_sent=… acked=… expired=… ack_miss_peer=… ack_miss_key=…
```

Se lee de izquierda a derecha; el primer cero es el salto roto. Ojo a que
`emitted`/`acked` son de MI lado (yo genero para ese peer) y `recv`/`ack_sent`
del contrario (él genera para mí), así que el diagnóstico completo necesita el
log de las dos máquinas:

| lectura | dónde está roto |
|---------|-----------------|
| `emitted=0` | no genero: sin rate del SDN, sin peers, o `generator.enabled=false` |
| `emitted>0` aquí y `recv=0` en el peer | la clave se pierde en el ORR/QKC de ida — mira `orr.incoming master_secret missing` y `qkc.relay.handle_err` |
| `recv>0` en el peer pero su `ack_sent=0` | no supo a dónde acusar recibo (`ack_no_endpoint`) |
| su `ack_sent>0` y mi `acked=0` | el ACK no llega: 20009 filtrado, o le anuncié un `ack_endpoint` que no me alcanza |
| `ack_miss_peer>0` | su `node_id` no coincide con la clave `[peers.<id>]` de mi `node.yml` |
| `ack_miss_key>0` | los ACK llegan tarde: sube `generator.ack_timeout_ms` |

Cuando `buffer_enc` está a cero se emite además una línea `generator.diag` que
traduce los contadores a la causa concreta. Y al arrancar, el DKMS valida su
propio `ack_endpoint` (avisa si es `0.0.0.0`/loopback y lo sondea por TCP): si
esa comprobación falla, ningún peer podrá acusar recibo jamás.

## Notas multi-institución y límites conocidos

- **SDN central única**: es el modelo soportado hoy; federación por
  institución no existe todavía.
- **Enlaces QKD, sí manuales**: los `pqc` los crea la SDN en caliente, pero un
  `qkd` necesita el `kme_url` local en el `node.yml` de los dos extremos.
- **La tasa de un enlace es compartida** entre ambos sentidos (no hay modelo
  A→B / B→A separado).
- **Sin límites de RAM/CPU** en los compose (default de Docker). En máquinas
  pequeñas añade `mem_limit:` al servicio.
- Los buffers de claves del DKMS viven **solo en RAM**: un reinicio los vacía
  y el generator los rellena de nuevo (por diseño; no hay persistencia de
  material de clave).
