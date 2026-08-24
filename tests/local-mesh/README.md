# Malla local de N nodos

Levanta un despliegue DKMS completo —SDN + N×(qkc, orr, dkms)— en una sola
máquina, a partir de los mismos `node.yml` y el mismo `render_config.py` que
usan las imágenes. Ejercita el contrato de despliegue de verdad, no una
configuración inventada para el test.

No sustituye a [`../testbed/`](../testbed/), que corre contra el despliegue
real por SSH y es donde se miden las cosas que dependen de la red. Esto es lo
que se puede repetir sin hardware, en minutos, antes de tocar el testbed.

```bash
cargo build --release
./mesh.sh up 10            # levanta la malla
./mesh.sh keys             # intercambio ETSI-014 por par ordenado
./mesh.sh down
```

## Qué comprueba

`mesh.sh keys` es el que importa. Por cada par **ordenado** de DKMS: el SAE
maestro pide `enc_keys` en su DKMS, el esclavo recupera esa `key_ID` con
`dec_keys` en el suyo, y se comparan los bytes. La comparación es el punto —
la clave de sesión viaja envuelta en OTP con una clave de transporte y no
lleva integridad propia, así que un desalineamiento del material entregaría
claves distintas a los dos SAE sin que nada fallase. Ordenados y no
desordenados porque el `buffer_enc` de un extremo es el `buffer_dec` del otro:
A→B y B→A gastan material distinto y pueden fallar por separado.

Con N=10 son 90 intercambios; tarda unos 15 s.

## Topología

`mesh.sh up N [ring|star|random]`, y la elección cambia lo que se mide:

| | Qué monta | Para qué |
|---|---|---|
| `ring` (default) | anillo + cuerdas; con N=10, 14 aristas y grados 2-3 | el caso amable: conexa, sin puntos únicos de fallo y con caminos alternativos para el multipath |
| `star` | todos colgando del nodo 1; grados 9 y 1 | carga un solo QKC con todo lo que no sea suyo. **Quitar el hub parte la red**, así que las bajas de nodo hay que hacerlas sobre una hoja |
| `random` | grafo conexo con semilla (`DKMS_MESH_SEED`, default 42); con N=10, 15 aristas y grados 2-6 | lo que se parece a un despliegue real, donde nadie cablea un anillo perfecto |

La aleatoria se construye como árbol de expansión aleatorio más aristas extra
hasta grado medio ~3: un `G(n,p)` a secas puede salir partido, y una malla
partida no mide lo que se quiere medir, mide otra cosa.

En las tres, cada arista la declara **un solo extremo** a propósito: es lo que
comprueba que la SDN se lo comunica al otro (ver "Peers ride back on the
announcement" en `CLAUDE.md`).

Medido en local con N=10, los 90 pares ordenados dan bytes idénticos en las
tres, y la malla queda operativa —todos los buffers llenos— en unos 75 s.

## Enlaces QKD simulados

Por defecto los enlaces son PQC: los QKC derivan las claves entre sí con
ML-KEM. Con `DKMS_MESH_LINK_TYPE=qkd` se levanta **un `quditto` por arista** y
los QKC sacan el material de él por ETSI-014.

```bash
DKMS_MESH_LINK_TYPE=qkd ./mesh.sh up 10 ring
```

Dos diferencias que no son de detalle:

- **Los dos extremos declaran el enlace.** La SDN no puede suministrar el
  `kme_url` —no sabe dónde está el KME de esa institución—, así que un enlace
  declarado por un solo lado quedaría a medias. En PQC se sigue declarando por
  un solo extremo a propósito.
- **La capacidad de la arista pasa a significar algo.** En PQC lleva un
  centinela de 1e9 y λ no tiene nada que la acote; en QKD la SDN la dimensiona
  con `cap = R0·10^(−α·d/10)`. Con los defaults (`R0=2000`, `α=0.2`, `d=5km`)
  salen **1588,7 claves/s por arista**, y `mesh.sh up` lo imprime al arrancar.
  Ojo: **la capacidad NO es R0**, y confundirlos ha costado más de un "déficit
  contra el teórico" que no lo era.

Los dos QKC de un enlace apuntan al mismo quditto: uno pide `enc_keys` y el
otro recupera esas mismas con `dec_keys`, así que **compiten por la misma FIFO**
— `R0` es por enlace, no por sentido. Se ajusta con `DKMS_MESH_R0`,
`DKMS_MESH_ALPHA` y `DKMS_MESH_DIST_KM`.

## Medir

```bash
./stress.sh --arm A                 # defaults de fábrica
./stress.sh --arm B --tokens 3200   # con el techo levantado
./bootstrap_times.py                # tiempos de convergencia de la malla viva
```

`stress.sh` carga **todos los pares ordenados a la vez** (90 con N=10) y saca
tres tramos por punto del barrido: ráfaga con los buffers llenos —que mide la
ruta de servicio, mTLS + OTP + ETSI-020, sin el generador de por medio—,
sostenido una vez drenado el buffer —que mide la cadena DKMS→ORR→QKC— y
recuperación. Reutiliza `../testbed/sae_load.py` como cliente de carga.

**Las dos ramas existen porque con los defaults el techo no es del sistema, es
nuestro**: `tick_ms=100` × `max_tokens_per_peer_per_tick=32` son 320 claves/s
por peer. La rama A caracteriza lo que se lleva un operador; la B levanta ese
techo y desactiva el bucket por SAE para que aparezca el límite real. Medido
en local con N=4 y 2 hilos por par: 243,8 claves/s por par sostenidas contra
las 320 del techo, con p50 = 2,9 ms y p99 = 9,3 ms.

### Lo que salió en CESGA (10 nodos, 2026-08-21)

Cuatro campañas en nodos de cómputo del FT3, ~25 min cada una:

| Topología | 1 hilo/par | 4 hilos/par | 16 hilos/par |
|---|---|---|---|
| anillo | 7 773 claves/s | **8 077** | 5 702 |
| estrella | 7 855 claves/s | **8 146** | 5 512 |
| aleatoria | 7 759 claves/s | **8 059** | 6 048 |

Las tres se comportan igual, y que la estrella no sea peor es un resultado:
el hub queda con grado 9 pero el transporte DKMS↔DKMS es PQC extremo a
extremo y no se apoya en el grafo. El óptimo está en 4 hilos por par; a 16 la
tasa **cae** y la latencia p50 pasa de 7,5 a 39 ms — sobresaturación, no
rotura: sigue sirviendo con la integridad intacta.

7 800 claves muestreadas con `dec_keys` bajo carga, todas con bytes
idénticos. `recv_corrupt = 0` en las tres mallas y los 30 ORR con
`dropped_no_secret = 0` y `peel_failed = 0`. La malla queda operativa en 70 s.

**La rama B enseñó que el techo por defecto es lo que mantiene el sistema
estable.** A 1 hilo por par dio 21 714 claves/s agregadas (241 por par) y
**cero rechazos**, casi el triple. Pero a 4 hilos se derrumbó a 2 749, a 16 se
quedó en 32 y el trabajo murió por OOM con 16,6 GB, frente a los ~6 GB
estables de la rama A. Sin ese tope el generador produce más rápido de lo que
la cadena ORR→QKC transporta y las colas crecen sin freno: **falta
contrapresión entre el generador y el transporte**, y el tope la estaba
supliendo.

El informe atribuye los rechazos él solo, que es lo que evita sacar
conclusiones falsas: un `429` con el buffer ENC vacío es contrapresión
legítima —la demanda supera al refill—, no un límite de capacidad. Por eso el
muestreo recoge también la rate que la SDN asigna, que en un despliegue
PQC-only no significa nada y puede caer a cero.

Y muestrea la integridad bajo carga: recupera con `dec_keys` una muestra de
las claves servidas y compara el sha256. Una sola discrepancia importaría más
que toda la curva de throughput.

`bootstrap_times.py` no instrumenta nada: se apoya en las líneas periódicas de
cada módulo y en `starts.tsv`, que `mesh.sh` escribe con el instante exacto en
que lanza cada proceso — el t0 es el lanzamiento y no el primer log, porque
entre uno y otro está la inicialización, que es parte de lo que se mide.

## Cambiar la topología en caliente

`mesh.sh link <n> [vecinos...]` reescribe los vecinos que declara un QKC y lo
reinicia. Con eso se crean, modifican y eliminan enlaces, porque el anuncio es
autoritativo sobre lo que ese QKC declara: dejar de nombrar a un vecino retira
la arista si el otro extremo tampoco la declara.

```bash
./mesh.sh link 2 3 7       # el qkc2 pasa a tener fibra con el 3 y el 7
./mesh.sh link 2 3         # ...y suelta la del 7
./mesh.sh edges            # aristas + comprobación de conectividad
```

Para dar de baja un nodo entero, mata sus tres procesos y espera al
`presence_ttl_secs` (90 s por defecto): la SDN lo saca con sus aristas y el
resto se entera solo.

## Memoria

Todo corre dentro de un scope de systemd con `MemoryMax=8G`
(`DKMS_MESH_MEM_MAX` para cambiarlo). Donde no se puede crear el scope —dentro
de un trabajo de SLURM no hay bus de sesión de usuario— se arranca sin él y se
avisa: ahí el tope es el `--mem` del trabajo, que es un cgroup igual de real.
Fuera de SLURM y sin scope no hay tope ninguno, y el script lo dice.

El tope importa: un despliegue local se ha comido una sesión de escritorio
antes — ver la sección de saturación del `CLAUDE.md`. Con N=10 el pico medido
son 0,46 GB, así que los 8 GB son margen, no restricción.

## Dónde deja las cosas

En `tests/results/local-mesh/` (gitignored): configs renderizadas, certificados
y logs de los 31 procesos. `mesh.sh up` lo borra y lo rehace, así que no
guardes nada ahí que quieras conservar.

Los certificados los emite `docker/gen-certs.sh` con una CA única para toda la
malla, que es lo que exige el mTLS entre DKMS. Los de cliente SAE llevan la
identidad en el SAN (`URI:urn:dkms:sae:<id>`), de donde la saca el DKMS.

## Límites conocidos

- **N entre 2 y 10.** Los puertos son `20000 + 100·(n-1)`, así que a partir de
  11 se pisarían con otros rangos.
- **Todo es PQC.** No hay enlaces QKD ni quditto: harían falta un `kme_url` y
  un servicio detrás.
- **Un solo host.** No mide nada de latencia ni de red, y el aviso de
  `ack_endpoint … loopback` en los logs del DKMS es esperado aquí.
