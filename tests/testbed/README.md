# Plan de pruebas — testbed Proxmox

Batería de pruebas sobre el despliegue real (Docker/compose multi-máquina) del
hipervisor `castor`. Cubre lo que hoy no está verificado de forma repetible:
que se pueden **pedir claves** (funcional y bajo carga), que se pueden
**añadir y quitar módulos en caliente** —la razón de ser de la rama
`auto_conf_peers`— y que el enlace PQC **se recupera** de los reinicios que se
arreglaron en los últimos commits.

Ninguno de estos scripts modifica el código. Todos son idempotentes salvo los
de la fase 3, que despliegan y retiran un nodo (y lo dejan como lo encontraron).

---

## Estado del testbed a 2026-08-01

Verificado por inspección directa antes de escribir esto:

| VM | IP | rol | contenedores | imagen |
|----|----|-----|--------------|--------|
| 201 `dkms-sdn` | 192.168.50.201 | SDN central | `sdn-sdn-1` (22 h) | `sdn:auto-peers-v1` |
| 202 `dkms-node-a` | 192.168.50.202 | qkc+orr+dkms (id 1) | 3 (19 h) | `*:diag-v7` |
| 203 `dkms-node-b` | 192.168.50.203 | qkc+orr+dkms (id 2) | 3 (19 h) | `*:diag-v7` |
| 204 `dkms-node-c` | 192.168.50.204 | qkc+orr+dkms (id 3) | 3 (19 h) | `*:diag-v7` |
| 205 `dkms-builder` | 192.168.50.205 | buildx | **apagada** | 32 c / 24 GB |

Hipervisor: 96 cores, 251 GB (239 libres), `local-lvm` al 4 %. Sitio para las
1-2 VMs nuevas de la fase 3 sin apretar nada.

Salud actual: topología `{"dkms":3,"edges":3,"orrs":3,"qkcs":3,"saes":3,"version":15}`,
triángulo 1–2–3 con enlaces PQC. En node-a: `ack_pending=0`, `recv_corrupt=0`,
buffers `enc=4124` con los dos peers. `sdn_rate_keys_per_s=0.0` porque **no hay
ni un SAE pidiendo claves**: el sistema lleva 19 h en vacío.

### Tres cosas que condicionan el plan

1. **Las imágenes desplegadas no son HEAD.** Los nodos corren `diag-v7` y la
   SDN `auto-peers-v1`, tags de diagnóstico de hace ~20 h. Los dos últimos
   commits (`qkc: renegotiate the link when the peer reconnects`, `qkc: notice
   a peer that hung up while the link was idle`) casi con seguridad no están
   dentro. **Medir resiliencia PQC contra estas imágenes no dice nada del
   código actual** → fase 0 obligatoria antes de la fase 4.

2. **Los `node.yml` de a/b/c declaran todos sus peers a mano** (links a 1 y 2,
   `peers: orr_1, orr_2`, `peers: dkms-1, dkms-2`). Es decir: el suelo local
   cubre el 100 % de la topología y **el camino auto-peer de la SDN no se está
   ejercitando**. Es correcto como despliegue, pero como test es un falso
   verde. La fase 3 lo ataca por el único sitio donde se nota: un nodo nuevo
   que **nadie** tiene declarado.

3. **No hay certificados de SAE en ninguna VM.** Solo `ca.crt` +
   `dkms-N.crt/key`. La CA viva está en `dkms-sdn:~/site4/certs/ca.key` (ojo:
   **no** es la CA que hay commiteada en `tests/results/proxmox-docker-smoke/`,
   los fingerprints difieren). Sin certs de SAE no se puede pedir una sola
   clave → `provision_certs.sh` es requisito de todo lo demás.

### Qué encontró la primera ejecución (2026-08-02)

La batería se ejecutó por primera vez el 2026-08-02 sobre imágenes construidas
desde `9a6ba14`. Encontró **nueve** cosas rotas: seis en el producto, dos en el
tooling de build/despliegue y una en el propio plan. Todas corregidas. Las dos
primeras de la tabla son las graves, y ninguna de las dos se ve en `docker ps`
ni en la topología de la SDN: hay que cruzar los dos extremos de un par
(`emitted ↔ recv`, `recv_corrupt`) o mirar `RestartCount` y `panicked at`.

| # | Dónde | Qué pasaba | Arreglo |
|---|-------|------------|---------|
| 0 | `orr/src/bootstrap.rs` | dar de alta un nodo dejaba un par de ORRs con `master_secret` distinto en cada extremo → **el 100 % de las claves de transporte de ese par llegaban corruptas**, en los dos sentidos | un solo `encap` en vuelo por par: el bootstrap del anuncio toma el mismo guard que el re-bootstrap pasivo |
| 0b | `orr/src/qkc_link.rs` | `select!` sin `biased` sondeaba un `JoinHandle` ya completado → panic de tokio y, con `panic = "abort"`, **el ORR se mata solo**: 10 reinicios en un nodo, cada uno rehaciendo el bootstrap con todos sus peers | `biased;` con la rama del reader primero, para salir del bucle antes de poder repolarla |
| 1 | `dkms/src/service.rs` | `/status` anunciaba `max_key_per_request=64` y nadie lo comprobaba: `number=65` devolvía 65 claves, y nada impedía pedir un millón | `check_request_limits`, con las constantes compartidas entre lo que se anuncia y lo que se exige |
| 2 | `dkms/src/service.rs` | `size=7` devolvía **en silencio** una clave de 8 bits — distinta de la pedida, y un SAE no tiene forma de notarlo | rechazo 400 si `size` no es múltiplo de 8 o cae fuera de `[min,max]` |
| 3 | `dkms/src/service.rs` | `size=100000` acababa en **HTTP 500** en vez de un rechazo limpio | mismo rango; ya no llega a la ruta que reventaba |
| 4 | `docker/gen-certs.sh` | el cert del DKMS no llevaba `IP:127.0.0.1` en su SAN, así que un SAE co-locado apuntando a `https://127.0.0.1:20005` —lo natural— moría en el handshake TLS | SAN con `IP:127.0.0.1` además de `DNS:localhost` |
| 5 | `scripts/build-images.sh`, `scripts/deploy-images.sh` | `make images` construía con `Dockerfile.workspace`, que produce el binario pelado **sin** `entrypoint.sh`: el despliegue multi-host entra en crash-loop (`qkc: required arguments were not provided: --config`) | ambos apuntan ya a `docker/Dockerfile`; aviso en la cabecera del workspace |
| 6 | este plan | contar el **503** como fallo hacía fallar T20 justo cuando el sistema se comportaba bien: en ETSI-014 el 503 es el "no key available" correcto al vaciarse el buffer | el criterio distingue 500/502/504 (fallo) de 429/503 (degradación) |

Y tres cosas del propio harness que daban falsos resultados o lo colgaban:

- `ssh` sin `-n` dentro de un `while read` se bebía la entrada del bucle, así
  que el muestreo de integridad comprobaba **1** clave en vez de 20.
- Un `|| echo 000` de más convertía el `000` de un rechazo TLS en `000000`,
  que no casaba con ningún patrón y se leía como "el mTLS no está exigiendo
  cert" cuando sí lo hacía.
- **Lanzar un proceso remoto en segundo plano sobre una conexión SSH
  multiplexada cuelga el lanzador**: el `ssh` que arranca la carga se queda
  esperando a que cierre el canal y no vuelve — se le vio parado 17 min con la
  carga ya terminada. Va por `on_detached` (`ControlMaster=no` +
  `setsid … < /dev/null`), que vuelve en 0,07 s. El multiplexado se queda para
  el resto, donde sí hace falta: el muestreador de T20 abre una docena de
  conexiones cada 5 s.

**Pendiente en el harness**: `--aggregate-throttled` de `sae_load.py` no está
surtiendo efecto — a 16 hilos el CSV seguía trayendo 1 049 029 filas de 429 una
a una (113 MB por nodo y punto) en vez de un resumen por segundo. No afecta a
las cifras, solo al volumen y al tiempo de `scp`; queda por depurar.

### El despliegue NO está sano, aunque lo parezca

Esto salió al ejecutar `t00_health.sh` (que es de solo lectura) mientras se
escribía el plan. La topología de la SDN está perfecta, los tres QKC tienen sus
enlaces PQC vivos y `recv_corrupt = 0` en todos: por ahí no se ve nada. El
cruce entre los dos extremos de cada par sí:

```
dkms-node-a → dkms-3: emitted=5408    expired=1280     el peer dice recv=0        ← NADA LLEGA
dkms-node-b → dkms-3: emitted=6688    expired=2560     el peer dice recv=0        ← NADA LLEGA
dkms-node-c → dkms-1: emitted=254494  expired=250398   el peer dice recv=19016    ← 98% expiradas
dkms-node-c → dkms-2: emitted=254421  expired=250325   el peer dice recv=16968    ← 98% expiradas
```

**dkms-3 no ha recibido un solo `DKMS_BUFFER` en 19 h** (`recv=0`, `dec=0`,
`peer_ack_endpoint="<sin recibir>"` con sus dos peers), y de lo que él emite
llega el 7 %. El propio DKMS lo dice, y con el diagnóstico ya escrito en el
código:

```
generator.diag: buffer_enc vacío — sus ACK llegan TARDE: el reaper ya había
  expirado la clave. Sube generator.ack_timeout_ms o baja la rate de emisión
generator.emit batch had failures peer=dkms-1 ok=0 failed=32
  sample_error="orr send_message: status: Cancelled, message: \"Timeout expired\""
```

Las últimas de esas líneas son de 2026-07-31T16:48, una hora después del
arranque; desde entonces todo está callado porque sin SAEs no hay demanda y los
buffers ENC ya están llenos. O sea: **el fallo está congelado, no resuelto**.
Los contenedores no se han reiniciado (`RestartCount=0`, arranque 15:46-15:48
del 31/07), así que no es un artefacto de contadores.

Dos consecuencias para el plan:

1. **La fase 1 va a fallar en cuanto se le pida una clave a dkms-3** — es la
   prueba de que el problema sigue vivo, no un fallo del test.
2. **T00 tal como estaba lo daba por bueno**: 20 PASS, 0 FAIL mirando cada nodo
   por separado. Por eso ahora incluye la reconciliación `emitted ↔ recv` entre
   los dos extremos y la comprobación de `peer_ack_endpoint`. Un fallo mudo que
   solo se ve cruzando dos logs es exactamente lo que esta batería tiene que
   cazar.

**Diagnosticado el 2026-08-02, y no era un bug nuevo.** Los contenedores de
node-c se habían reiniciado **solos**, 95 s después de los de a y b (su log del
ORR contiene dos `orr starting`, a las 15:46:59 y a las 15:48:34; `docker
restart` conserva el log del contenedor anterior, de ahí que `RestartCount`
siga a 0). Eso disparó las dos cosas a la vez:

- El ORR de c reapareció sin `master_secret`, y sus pares siguieron cifrando
  con el epoch viejo: `orr.incoming master_secret missing for epoch (drop)
  from=orr_1 epoch_id=0 latest=None`. Todo lo que a y b le mandaban se caía
  ahí — es la carrera de bootstrap del ORR que CLAUDE.md ya documenta, con su
  workaround: reiniciar el despliegue entero.
- El QKC de c se quedó sin claves ENC para sus dos vecinos durante una hora
  (`wait_enc_batch.timeout peer=1 missing=1 enc_len=0` ×241 612, más
  `enc_refill failed … timeout waiting pqc-secret`), lo que hacía que los
  `send_message` del DKMS agotaran su `rpc_timeout` y sus claves expiraran.

Reconstruir las imágenes y **levantar los tres nodos a la vez** lo dejó limpio:
T00 pasó a 27/27 con los seis pares reconciliando exacto (4096 emitidas = 4096
recibidas, 0 expiradas). La lección operativa está en la fase 0: **nunca
reiniciar un nodo suelto** de este despliegue.

### Dos anomalías menores que las pruebas deben acotar

- **`dec=19012` para el peer dkms-3 en node-a**, contra `capacity_per_peer =
  4096`. No es un bug per se: `SecureKeyBuffer::try_push` documenta que la
  capacidad es *soft-hint* y que el push nunca rechaza, porque la RAM «se acota
  porque el generator y el receptor ORR están limitados por la rate del SDN y
  los SAEs van drenando». En este testbed **no hay SAEs drenando**, que es
  justo la pata del argumento que no se cumple, y el buffer va por 4,6× su
  capacidad nominal. T12 mide si crece sin techo.
- **`misses=1721`** en el QKC de node-a hacia el peer 3 con el enlace en
  reposo. T00 lo deja registrado como línea base y T20 mira si escala con la
  carga.

---

## Cómo se ejecuta

```bash
cd tests/testbed
./provision_certs.sh           # una vez: emite y reparte todo el TLS
./t00_health.sh                # línea base — correr siempre antes y después
```

Cada script imprime `PASS`/`FAIL` por comprobación y sale con código ≠ 0 si
algo falla. Los artefactos van a `tests/results/<campaña>/` (gitignored).

Requisitos: `~/.ssh/config` con los hosts `dkms-sdn|dkms-node-a|b|c` y
`proxmox` (ya está), `openssl`, `jq`, `python3`.

---

## Fase 0 — Alinear el testbed con el código

**Por qué**: sin esto, la fase 4 mide binarios de anteayer.

```bash
TAG=head-$(git rev-parse --short HEAD)
docker buildx bake -f docker/docker-bake.hcl --set '*.platform=linux/amd64' \
    --load qkc orr dkms sdn                          # ~8 min desde cero
for h in dkms-node-a dkms-node-b dkms-node-c; do
    docker save pablopio/{qkc,orr,dkms}:$TAG | ssh $h docker load
    ssh $h "cd site && sed -i s/^TAG=.*/TAG=$TAG/ .env && docker compose -f site.yml up -d"
done
# la SDN igual, con sdn.yml en ~/sdn
```

Tres cosas que cuestan una tarde si no se saben:

- **La VM 205 (`dkms-builder`) no tiene salida a internet**: no resuelve
  `registry-1.docker.io`, así que el build ahí falla al bajar la imagen base.
  Se construye en el portátil.
- **Reiniciar los tres nodos a la vez, no de uno en uno.** Reiniciar un nodo
  suelto dispara la carrera de bootstrap del ORR y deja el despliegue medio
  muerto durante horas sin que la topología de la SDN se entere (es
  exactamente lo que había pasado el 31/07, ver abajo).
- **Después de tocar certificados hay que reiniciar los DKMS**, que los leen
  al arrancar.

**Criterio**: `docker ps` en las 4 VMs muestra el tag nuevo, y T00 vuelve a
pasar entero. Si T00 falla justo después del redespliegue y pasaba antes, la
regresión está en los commits nuevos, no en el testbed.

**No automatizo esta fase**: publicar imágenes y reiniciar los cuatro nodos es
una acción con efecto sobre un despliegue vivo y quiero que la lance una
persona.

## Cómo se provisiona el TLS

```bash
./provision_certs.sh          # emite lo que falte y lo reparte
./provision_certs.sh --force  # rehace el PKI entero
```

Emite la CA, el cert de servidor de cada DKMS (SAN `URI:dkms://<id>` + su IP +
`IP:127.0.0.1` + `DNS:localhost`), los certs de cliente de los SAE y una CA
ajena para el caso negativo de T11; luego reparte a cada VM su cert y **todos**
los de SAE, porque los scripts necesitan actuar como cualquier SAE desde
cualquier máquina.

Detecta solo el caso que se dio el 2026-08-02: una `ca.crt` y una `ca.key` que
no se corresponden. Si la clave de la CA en uso se pierde —y se perdió— no hay
más salida que reemitirlo todo, lo que obliga a reiniciar los DKMS. El material
vive en `~/.dkms-testbed/certs`, **fuera del repo**.

---

## Fase 1 — Funcional: pedir claves

### T00 — Salud y línea base (`t00_health.sh`)

Recoge, sin tocar nada: topología de la SDN (`/topology`, `/qkcs`, `/orrs`,
`/dkms`, `/saes`, `/links`), `/stats` de cada QKC, último `generator.state` de
cada DKMS y último `keystore.levels` de cada QKC. Lo vuelca a
`tests/results/<campaña>/baseline/`.

**Criterio de aceptación**
- `qkcs = orrs = dkms = 3` y `edges = 3`.
- Todos los `generator.state` con `ack_pending` estable y `recv_corrupt = 0`.
- Cada QKC con `handshake.established` hacia sus dos vecinos y sin
  `enc_refill failed … timeout waiting pqc-secret` en los últimos 5 min
  (esa línea + `enc=0 dec=0 taken=0` es la firma del bug de reinicio PQC).
- `peer_ack_endpoint` conocido para todos los peers de todos los DKMS.
- **Reconciliación entre extremos**: para cada par ordenado, lo que X dice
  haber emitido hacia Y aparece en el `recv` de Y, y no expira más de la mitad.
  Esta es la comprobación que caza los fallos mudos: los contadores de un nodo
  pueden verse impecables mientras su pareja no recibe nada (ver arriba).

Estado hoy: **FAIL** por este último criterio, y es correcto que falle.

### T10 — Una clave, extremo a extremo (`t10_keys_smoke.sh`)

El smoke de `docker/README.md` convertido en aserción. Para cada par ordenado
de los 3 nodos (6 flujos): `sae_i` pide `enc_keys` a su DKMS local, `sae_j`
recupera esa `key_ID` con `dec_keys` en el suyo, y se **comparan los bytes**.

**Por qué importa más de lo que parece**: CLAUDE.md deja escrito que la clave
de sesión del SAE **no lleva ninguna comprobación de integridad** —se envuelve
por OTP con una clave de transporte y viaja por ETSI-020—, así que si una clave
de transporte se desincronizara entre los dos extremos, los dos SAEs se
llevarían claves distintas *sin que nadie se entere*. El `key_digest` del
`DKMS_BUFFER` (commit `f4e6570`) cierra esa puerta por la vía del generator,
pero no en principio. Comparar bytes es la única verificación real que existe.

**Criterio**: 6/6 flujos con `enc_keys` 200, `dec_keys` 200 y bytes idénticos.
Cualquier par con bytes distintos es un fallo de severidad máxima, no un flake:
guardar los logs de los 3 nodos y parar la campaña.

### T11 — Rechazos y bordes (`t10_keys_smoke.sh --edges`)

Casos que deben fallar *bien*:

| caso | esperado |
|------|----------|
| `dec_keys` de una `key_ID` inventada | 404 / error ETSI, no 500 |
| `dec_keys` de la misma `key_ID` dos veces | la segunda no entrega clave |
| `enc_keys` hacia un `slave_sae` que no existe | error claro, no cuelgue |
| `enc_keys` sin cert de cliente | rechazo TLS |
| `enc_keys` con cert de otra CA | rechazo TLS |
| `number` > `max_key_per_request` de `/status` | error ETSI, no truncado mudo |
| `size` distinto de 256 | coherente con lo que anuncia `/status` |

**Criterio**: ninguno devuelve 5xx ni deja el DKMS en estado raro (T00 pasa
después). Se anota la desviación cosmética ya conocida (`master_SAE_ID: ""` en
`GET /status`) pero no bloquea.

---

## Fase 2 — Carga

### T20 — Ramp de SAEs (`t20_load.sh`, `sae_load.py`)

`sae_load.py` corre **en cada VM de nodo** contra su DKMS local
(`127.0.0.1:20005`, que es donde vive un SAE de verdad), con mTLS y keep-alive,
N hilos pidiendo `enc_keys` en bucle. Registra por petición
`t_unix,status,latency_ms,n_keys`.

Tres tramos, y el interesante es el segundo:

1. **Ráfaga (t=0..30 s)** — los buffers están llenos (~4096/peer). Mide cuántas
   claves se sirven "gratis" y con qué latencia.
2. **Sostenido (t=30..300 s)** — vaciado el buffer, el ritmo lo marca el refill
   del generator, que a su vez depende de la rate que asigna la SDN y del
   `max_tokens_per_peer_per_tick = 32` (techo duro 320 kps/peer). **Esta es la
   cifra que responde "¿aguanta carga alta?"**, no la ráfaga.
3. **Recuperación (t=300..420 s)** — parada de la carga; cuánto tarda el buffer
   en volver a 4096.

Concurrencias: 1, 4, 16, 64 hilos por nodo (barrido; cada punto 5 min).

**Qué se mide**
- Tasa de éxito sostenida (claves/s) por flujo y agregada.
- Latencia p50/p95/p99 por tramo.
- Desglose de errores por código.
- En paralelo, cada 5 s: `generator.state` (enc, dec, `ack_pending`,
  `emit_failed`, `expired`, `enc_full`, `recv_corrupt`) y `keystore.levels`
  (`enc`, `dec`, `taken`, `misses`, `wenc_to`, `wdec_to`) de los 3 nodos, más
  `/rate/:dkms_id` y `/demand` de la SDN.

**Criterio de aceptación**
- 0 respuestas **500/502/504**. En cambio 429 y 503 **son** el comportamiento
  correcto bajo saturación: el 429 es el control de admisión, y el 503 es el
  "no key available" que ETSI-014 define para cuando el buffer se vacía más
  rápido de lo que el generator lo rellena. Contar el 503 como fallo —el
  criterio con el que empezó este plan— hace fallar el test justo cuando el
  sistema se está comportando bien.
- `recv_corrupt = 0` en todo el barrido.
- La tasa sostenida no colapsa al subir la concurrencia: de 16 a 64 hilos el
  agregado debe quedarse plano, no caer. Una caída = falta backpressure en
  algún punto (es exactamente el patrón del bug `spawn-then-acquire` que ya se
  arregló una vez en el QKC).
- `ack_pending` acotado y `expired` que no crece monótonamente.
- La memoria RSS de los contenedores no crece sin techo (`docker stats`).

**Comparar contra teoría, no contra R0**: la capacidad de arista es
`cap = R0·10^(−α·d/10)`, no `R0`. Con los enlaces PQC del testbed el modelo de
capacidad ni siquiera aplica (los `r0/alpha/distance_km` no están puestos en
los `node.yml`), así que el techo esperado es el de la cadena
DKMS-generator → ORR → QKC, no el óptico. Anotar el número medido; no
declarar "déficit" contra ninguna cifra teórica sin haber calculado antes cuál
es la que toca.

### T12 — Reposo largo (`t20_load.sh --idle 3600`)

Sin ninguna carga, una hora, muestreando `dec`/`enc` y RSS cada 30 s. Existe
para acotar la anomalía del `dec=19012`: con `try_push` sin rechazo y sin SAEs
drenando, ¿el buffer DEC se estabiliza o crece?

**Criterio**: `dec` estable o con techo identificable. Si crece linealmente
durante una hora, es un consumo de RAM no acotado y hay que abrir un issue con
la traza.

---

## Fase 3 — Alta y baja de módulos en caliente

Es la funcionalidad de esta rama y la que menos verificación tiene.

### T30 — Añadir un nodo sin tocar los que corren (`t30_add_node.sh`)

Nodo D (`qkc_id 4` / `orr_4` / `dkms-4` / `sae_4`) en una VM nueva clonada de
`debian12-cloud-tmpl` (o, para una pasada rápida, en el `~/site4` que ya está
preparado en la VM de la SDN).

Lo que hace especial al test es **lo que el `node.yml` de D *no* dice**:

```yaml
# node.qkc.yml de D — un solo enlace declarado, hacia el nodo C
links:
  - { neighbor_id: 3, neighbor_addr: "192.168.50.204", type: pqc }
# node.orr.yml de D  → SIN peers ni peer_grpc_addrs
# node.dkms.yml de D → SIN peers
```

Todo lo demás lo tiene que traer la respuesta al anuncio: `orr_peers` y
`dkms_peers` son *todos los demás* (transporte E2E), y `qkc_peers` son los
vecinos en el grafo. Y sobre todo: **en a/b/c no se edita ni un fichero**.

**Secuencia**
1. `sha256sum` de los 9 `node.yml` de a/b/c → se vuelve a comprobar al final.
2. Snapshot de topología y de los enlaces de cada QKC.
3. `docker compose up -d` de D.
4. Esperar convergencia (≤ 3 heartbeats = 45 s con `sdn_announce_secs: 15`).

**Criterio de aceptación**
- SDN: `qkcs/orrs/dkms` 3→4, `edges` 3→4, `saes` 3→4, `version` sube.
- **node-c** (el único vecino de grafo de D) loguea en su QKC
  `enlace nuevo, dicho por la SDN` con `peer=4`, y lo registra en el
  forwarding (`/forwarding-table` del QKC de C incluye el 4). Sin eso el
  enlace existe y no se enruta — que es exactamente el bug que este código
  reemplazó.
- **a, b y c** loguean en su ORR `par nuevo: arranco su bootstrap` con
  `orr_4`, y sus DKMS incorporan `dkms-4` a `peers`.
- **a y b NO** crean enlace QKC hacia el 4 (no son vecinos de grafo).
- `handshake.established` entre QKC 3 y 4; `keystore.levels peer=4` con
  `enc > 0`.
- **e2e**: `sae_4` pide `enc_keys` hacia `sae_1` y `sae_1` la recupera con
  bytes idénticos. Esto ejercita además el multi-hop 4→3→1 en el forwarding.
- Los 9 `sha256sum` de a/b/c idénticos a los del paso 1.
- Los enlaces preexistentes (1–2, 1–3, 2–3) siguen arriba durante toda la
  operación: `keystore.levels` de a/b/c sin caer a `enc=0` en ningún momento.
  Este es el suelo que costó un bug de laboratorio (un QKC tirando sus dos
  enlaces propios a los segundos de arrancar).

### T31 — Quitar el nodo y comprobar que el suelo aguanta (`t31_remove_node.sh`)

`docker compose down` de D y esperar `presence_ttl_secs` (90 s por defecto).

**Criterio**
- La SDN lo retira: contadores vuelven a 3 y `version` sube.
- node-c **tira el enlace al 4** (el que le dio la SDN) y **conserva los suyos
  al 1 y al 2** (declarados en su `node.yml`). Esta asimetría es el invariante
  «`node.yml` es un suelo, no una foto»; si C pierde también sus enlaces
  locales, está roto.
- Los ORR loguean `par retirado por la SDN` para `orr_4`.
- T10 vuelve a pasar 6/6 después.
- Sin `panic` ni reinicio de contenedor en ninguna VM (`docker ps` con
  uptime continuo).

### T32 — Rearranque de un módulo suelto (manual, documentado)

Reiniciar **solo el ORR** de node-b. CLAUDE.md documenta que esto deja a los
ORR peers con `master_secret` viejos y el nuevo logueando «sin master_secret»
para siempre, con workaround «reiniciar el despliegue entero».

**No es un criterio de PASS/FAIL**: es un fallo conocido sin arreglo. El test
existe para **confirmar que sigue igual** y para medir cuánto tarda en
manifestarse, porque si algún día alguien implementa el re-bootstrap pasivo,
esta es la prueba que lo valida.

---

## Fase 4 — Resiliencia del enlace PQC

Requiere fase 0 hecha. Los tres casos que arreglan los últimos commits:

### T40 — Reinicia el extremo lex-mayor (`t40_restart_pqc.sh --greater`)

`docker restart site-qkc-1` en node-c (id 3) mientras 1 y 2 siguen arriba.
El iniciador es siempre el lex-menor, así que el 3 **no puede** pedir epoch
nuevo por su cuenta: el disparador es la reconexión TCP.

**Criterio**
- node-a y node-b loguean `qkc.pqc.relink: el peer se reconectó (¿reinicio?)`
  y después `qkc.pqc.relink completado`, en < 30 s.
- Los epochs nuevos son **estrictamente mayores** que los previos (nunca se
  reutiliza un número — un mismo número con secreto distinto en cada extremo es
  precisamente lo que nada aguas abajo puede detectar).
- `keystore.levels peer=3` vuelve a `enc > 0` y T10 pasa 6/6.
- **Firma de regresión**: `keystore.levels peer=N enc=0 dec=0 taken=0` +
  `enc_refill failed … timeout waiting pqc-secret` cada 10 s con
  `handshake.established` sano en ambos lados.

### T41 — Reinicia el extremo lex-menor (`t40_restart_pqc.sh --smaller`)

`docker restart site-qkc-1` en node-a (id 1). Aquí el que se reinicia es el
iniciador: manda INIT con pubkey nueva y el responder tiene que
**re-encapsular** en vez de reutilizar su ciphertext cacheado (`handle_init`).

**Criterio**: mismo que T40, más — el responder **no** debe loguear que
reutiliza una encapsulación previa. Si la reutiliza, los dos extremos divergen
en silencio y T10 empieza a devolver bytes distintos: ese es el único síntoma.

### T42 — Reconexión sobre enlace ocioso (`t50_idle_reconnect.sh`)

El caso del último commit. Con los buffers DKMS llenos **no hay escrituras**,
así que el writer no se entera de que el socket murió. Se fuerza el estado:
llenar buffers, **parar toda carga**, reiniciar un extremo, y esperar sin
generar tráfico.

**Criterio**
- El extremo vivo loguea `qkc.peer_out.peer_hung_up (EOF con la cola vacía)`
  en < 15 s, sin haber intentado escribir nada.
- Le sigue el relink, y el enlace queda operativo **sin** que nadie pida una
  clave para despertarlo.
- El rate-limit funciona: reiniciar el peer dos veces seguidas en < 5 s produce
  **un** relink, no dos (`RELINK_MIN_INTERVAL`), con la línea
  `qkc.pqc.relink omitido (demasiado seguido)`.

### T43 — Corte de red, no reinicio (manual)

`iptables -I INPUT -s <ip-vecino> -j DROP` en node-b durante 60 s y quitar.
Distinto del reinicio: los procesos siguen vivos con su estado.

**Criterio**: el enlace se recupera solo al restaurar la ruta. Un relink extra
aquí es aceptable y está documentado (cuesta tres handshakes ML-KEM y no rompe
nada); lo que no vale es quedarse con `enc=0` indefinidamente.

---

## Fase 5 — Integridad (transversal)

### T50 — El `key_digest` detecta corrupción

Único punto del camino con comprobación de integridad. Sin tocar el código no
se puede corromper un `DKMS_BUFFER` en vuelo desde fuera, así que:

- **En unit test** (ya existe en `dkms/src/southbound/orr.rs`): un bit cambiado
  se detecta y el digest está ligado al `key_id`.
- **En testbed**: verificar que `recv_corrupt` está expuesto en
  `generator.state` y vale 0 durante toda la campaña. Es un centinela: si algún
  día no vale 0, hay corrupción real en el OTP del enlace (que no lleva MAC).

Un test de inyección de corrupción real necesitaría un proxy TCP entre dos QKC
que voltee un bit. **Merece la pena y no está hecho** — queda propuesto, no
diseñado aquí.

---

### El alta de un nodo corrompía un par de ORRs (encontrado por T30)

T30 pasó 25 de 26 comprobaciones a la primera —incluida la que da sentido a
todo, los nueve `node.yml` de a/b/c intactos— y falló la última: `sae_4` no
conseguía una clave hacia `sae_1`. La causa resultó ser un bug de verdad.

El par **dkms-1 ↔ dkms-4 entregaba el 100 % de las claves de transporte
corruptas**, en los dos sentidos, mientras 4↔2 y 4↔3 iban perfectos. Lo cazó el
`key_digest` del DKMS, que es lo único que mira ahí abajo: sin él, los dos SAEs
se habrían llevado claves distintas sin que nada lo detectase.

Los timestamps del ORR lo cuentan entero:

```
orr_4  15:35:13.799  par nuevo: arranco su bootstrap orr=orr_1
orr_4  15:35:24.958  establish_secret bootstrap_secret stored peer=orr_1
orr_4  15:35:24.986  establish_secret bootstrap_secret stored peer=orr_1   ← el segundo, 28 ms después
orr_1  15:35:24.989  par nuevo: arranco su bootstrap orr=orr_4
orr_1  15:35:25.059  passive_rebootstrap start peer=orr_4
orr_1  15:35:25.081  bootstrap bootstrap_secret ok peer=orr_4
```

`orr_1` es el iniciador (la regla lex ya existía y funciona), pero **hizo dos
`encap` a la vez**: uno por el bootstrap del anuncio y otro por
`trigger_passive_rebootstrap`. Cada uno produce un `shared_secret` distinto;
`orr_4` guardó el último que le llegó y `orr_1` se quedó con el suyo. A partir
de ahí, todo lo que se cifre entre ellos sale ruido.

La raíz es un *check-then-act*: `bootstrap_peer` solo miraba
`if !peers.has_bootstrap(&peer_id)`, que no excluye al otro camino. El
re-bootstrap pasivo ya tenía su guard "in-flight"; ahora el del anuncio toma el
mismo, por intento y no durante todo el bucle de reintentos, para que el camino
pasivo —que además refresca la pubkey— pueda intervenir si el handshake no
converge (`orr/src/bootstrap.rs`).

De paso, el peso WCMP: en las rutas de ese par la SDN estaba publicando
`weight = 2 755 359 744`. No era un desbordamiento —cabe en un u32— sino
literalmente `flujo × 100` con los flujos disparatados que produce el LP cuando
λ se dispara. Pero el `as u32` de al lado **sí** habría envuelto en silencio por
encima de ~42,9 M claves/s, así que ahora satura (`sdn/src/mcmcf.rs`, con test).
Ese peso no causaba la corrupción: con un solo siguiente salto, `pick` lo
devuelve igual.

### Reinicios repetidos desincronizaban un enlace — arreglado (T41)

Un reinicio suelto de cualquiera de los dos extremos siempre se recuperó bien:
T40 lo pasa una y otra vez en ~8 s. Pero encadenando varias tandas (8 ciclos en
una sola sesión) un enlace acabó con los dos extremos derivando claves
distintas:

```
qkc-2 /stats, enlace al 3:  dec_lookups=130368  dec_misses=130368   ← el 100 %
dkms-3 → dkms-2: emitted=49152  acked=0  expired=45056  |  el peer dice recv=0
```

Todo lo que llega por ese enlace se descarta sin descifrar, para siempre. No se
auto-cura; reiniciar el despliegue entero lo limpia (comprobado: 0/4096 fallos
en los seis enlaces después). Y no se ve por ningún lado salvo mirando
`dec_misses`: los niveles de `keystore.levels` siguen siendo distintos de cero,
la topología de la SDN está perfecta y `recv_corrupt` es 0 —porque los frames
ni siquiera llegan a descifrarse, así que no hay digest que falle—.

La causa: un QKC que arranca **vuelve a numerar sus epochs desde 1**, mientras
su vecino conserva epochs mucho más altos. La invariante "un número de epoch no
se reutiliza nunca" se cumple dentro de la vida de un proceso, pero no a través
de un reinicio. Se observó directamente: recién levantado,
`handshake.established … epoch=1` con el peer guardando todavía epoch 17.

**El arreglo** aprovecha que la época viaja en los 4 primeros bytes de cada
`key_id`: el lado DEC, al descartar claves que no puede derivar, sabe
exactamente en qué ventana está el peer y pide resincronizar; el iniciador
renegocia un bloque **por encima de las dos** ventanas
(`SecretStore::request_resync` → `relink(peer_epoch)`).

Verificado con `t41_restart_storm.sh`, que reproduce la avería:

| | enlace a→3 tras 6 reinicios encadenados |
|---|---|
| antes (v7) | **19232/19232 fallos (100 %)**, permanente |
| después (v9) | se recupera solo; en estacionario **0 de ~890 lookups** |

Dos cosas que aprendí midiéndolo, y que están en el test:

- **La recuperación es reactiva**: un enlace roto y **ocioso** sigue roto hasta
  que alguien manda algo, porque la única evidencia es un frame indescifrable.
  Se cura a los segundos del primer tráfico. Un test que espere a que se arregle
  solo, en vacío, mide la avería y no el arreglo.
- **La métrica no es `dec_misses`.** Un miss solo dice que la clave aún no
  estaba materializada y `wait_dec` la resuelve enseguida — en estado sano se
  ven ratios del 20-30 % sin perder un frame. La pérdida real es
  `wait_dec_timeouts`, y el tráfico se cuenta con `dec_lookups`: en un enlace
  perfecto `wait_dec` no se llama ni una vez, así que usarlo de denominador
  hace que "perfecto" se lea como "sin datos".

**Queda un hueco**: solo el **iniciador** puede atender una petición de
resincronización. Si el único que detecta la divergencia es el respondedor, se
limita a dejarlo dicho en el log. En la práctica las ventanas se separan en los
dos sentidos y el iniciador también lo ve, pero una separación en un solo
sentido seguiría necesitando un reinicio manual.

## Pendiente para la próxima campaña (preparado el 2026-08-30, VMs apagadas)

Todo lo que sigue está en código y probado en malla local; lo que le falta es
la red real. Precondiciones: imágenes de `docker/Dockerfile` (ya con
`clang`/`libclang` para `highs-sys`), `provision_certs.sh` (ML-DSA), y en
cada `node.yml`: `sae_bindings` (obligatorio desde hoy), `ack_transport:
etsi020`, `ack_socket_listen: true`, `bootstrap_trust: strict`, sin
`control_addr`. Cliente de carga: `target-bookworm/release/sae_load` (t20 y
t41 lo suben a las VMs solos si existe; el Python exige OpenSSL ≥ 3.5).

1. **t00** — además de `RestartCount` y `panicked at`, un binario con el
   provider TLS equivocado o sin el híbrido ahora ABORTA en el arranque
   (`tls_pqc: self-check OK` tiene que estar en cada log).
2. **t10** con `sae_load --roundtrip`.
3. **Negativos nuevos** (uno por cambio de seguridad):
   - `Drain` desde otra VM contra `:20007` → conexión rechazada (se
     renderiza en `127.0.0.1`).
   - ACK forjado al socket `:20009` con `ack_socket_listen: true` → aceptado
     (documenta por qué debe irse); con `false` → rechazado.
   - `EstablishSecret` reclamando otro `orr_id` con un cert de DKMS
     (`orr/src/bin/test_client.rs`) → `PERMISSION_DENIED`.
   - `GetPublicKey` de un ORR con certs de una CA ajena
     (`provision_certs.sh` sabe emitirla) → `strict` lo rechaza
     (`orr.peer_pubkey: cadena del anuncio rechazada`).
   - `sae_load.py` en una VM con OpenSSL < 3.5 → se niega a arrancar.
4. **t20 en tres brazos**: `socket` + listener (base) → `etsi020` + listener
   → `etsi020` sin listener. Comparar `acked`, `expired`, `ack_send_failed`,
   keys/s y p95: es el on→off→delete de `docs/SECURITY.md` §Fase 4, y lo que
   decide el flip de defaults.
5. **t30/t31 en `strict`** — la propiedad clave del anclaje al cert: un nodo
   nuevo entra sin config por par.
6. **t40/t41** — relink (timer fuera del `select!`), resync pedido por el
   respondedor, y la rotación del ORR con un extremo reiniciado a mitad
   (`orr.rotation: el peer no tiene nuestro bootstrap_secret … rehago`).
7. **t50** y **soak ≥ 24 h** (12 h en reposo + 12 h con `sae_load` a baja
   tasa). Recuentos por hora de `qkc.pqc.rotation`, rekeys e2e,
   `orr.rotation success`, `recv_corrupt`, `bad_mac`, `replayed`, RSS. Pasa si
   las rotaciones QKC son floor(horas) ± 1 por iniciador, cero corruptos y
   RSS plano. Resultados a `tests/results/testbed-<fecha>/ANALYSIS.md`.

Después de eso, y solo después: `ack_transport = "etsi020"`,
`ack_socket_listen = false` y `bootstrap_trust = strict` por defecto, y el
borrado de `ack_socket.rs`.

## Resultados de la campaña `head-limits` (2026-08-02)

Sobre HEAD `9a6ba14` más los arreglos de la tabla de arriba, PKI reemitido y
los tres nodos levantados a la vez.

| test | resultado |
|------|-----------|
| T00 salud + reconciliación | **27/27** — los 6 pares ordenados cuadran exacto: 4096 emitidas = 4096 recibidas, 0 expiradas |
| T10 + T11 clave e2e y casos borde | **14/14** |
| T20 carga | ver abajo |
| T30 alta de nodo en caliente | **35/35** — incluidos `recv_corrupt = 0` en las seis direcciones con el nodo nuevo y la clave `sae_4→sae_1` por el camino 4→3→1 |
| T31 baja de nodo | **19/19** — node-c pierde el enlace que le dio la SDN y conserva los suyos |
| T40 + T41 reinicio PQC (los dos extremos) | **20/20**, recuperación en ~8 s |
| T42 reconexión en enlace ocioso | **6/6** — detecta el EOF con la cola vacía y se recupera sin que nadie pida una clave |

El sub-caso de `RELINK_MIN_INTERVAL` queda **inconcluso**, no verde: `docker
restart` tarda ~10 s en el ciclo y con `-t 0` el peer no llegó a reconectar dos
veces dentro de la ventana de 5 s. El test lo dice en vez de darlo por bueno.

### Carga: el techo es el token bucket, no el resto de la cadena

| hilos/nodo | sostenido (claves/s) | p50 | p95 | p99 | ≠200 |
|-----------|----------------------|-----|-----|-----|------|
| 1  | 948.4 | 0.6 ms | 0.7 ms | 1.0 ms | 429×1 896 345 |
| 4  | 950.3 | 1.1 ms | 2.2 ms | 44.6 ms | 429×2 020 005, 503×26 |

**Con un solo hilo por nodo el sistema ya está saturado.** La meseta cae en
948-950 claves/s agregadas, que es exactamente el techo del token bucket:
`max_tokens_per_peer_per_tick / tick_ms` = 32/100 ms = 320 claves/s por peer,
por 3 nodos = 960. Subir la concurrencia no mueve el sostenido —solo convierte
peticiones en 429—, que es lo que se quería comprobar: el backpressure aguanta.

Lo que **no** es el cuello, medido durante la carga: el QKC no falló ni una vez
(`misses=0`, `wenc_to=0`, `wdec_to=0` en las 300 muestras del keystore), ni ORR
ni QKC emitieron un solo WARN, `recv_corrupt=0`, `emit_failed=0`, `expired=0`,
cero panics y cero 500/502/504. Los 26 × 503 del punto de 4 hilos son el "no
key available" de ETSI-014 al drenar más rápido que el refill.

Para subir de ahí hay que tocar `max_tokens_per_peer_per_tick`, no la SDN:
en un despliegue solo-PQC su rate no acota nada (ver la nota de λ en CLAUDE.md).

## Orden sugerido

```
fase 0  (manual)                          ~15 min build + ~5 min despliegue
provision_certs.sh                        ~1 min  (y reiniciar los DKMS)
T00 → T10 → T11                           ~5 min
T20 (por punto: 3 min carga + 2 recup.
     + transferencia de CSV)              ~10 min/punto
T30 → T31                                 ~15 min
T40 → T41 → T42                           ~15 min
T00 otra vez (comparar con la línea base) ~2 min
T12 (una hora, en paralelo o al final)    ~60 min
```

Medido el 2026-08-02: T20 tarda mucho más de lo que dura la carga porque cada
punto arrastra >100 MB de CSV por nodo. Con 1 y 4 hilos ya se ve la meseta, así
que para una pasada rápida basta `--threads 4`.

Lo que **no** cubre este plan, y conviene saberlo:

- **Escala del LP de la SDN**. A 4 nodos el LP es trivial. Todo lo que hay
  escrito sobre el techo N≈20/N≥31 y el `Infeasible` falso de microlp sale de
  las campañas CESGA/EKS y de `problemas_escalado.md`, no de aquí. Si se quiere
  atacar eso, hace falta la opción de ~20 VMs que descartamos.
- **Enlaces QKD**. Todo el testbed es PQC; `quditto` no está desplegado. Los
  caminos `type: qkd` (incluido el aviso «la SDN anuncia un enlace QKD que no
  tengo configurado») quedan sin ejercitar.
- **Fairness / prioridades**. Con 3-4 nodos y una clase no se ve nada del
  solver híbrido HIGH/LOW.
