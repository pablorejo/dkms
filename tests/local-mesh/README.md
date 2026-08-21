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

Anillo más cuerdas: conexa por construcción, grado ≥2 para que quitar un nodo
no la parta, y con caminos alternativos para que el multipath tenga algo que
repartir. Cada enlace lo declara **un solo extremo** a propósito: es lo que
comprueba que la SDN se lo comunica al otro (ver "Peers ride back on the
announcement" en `CLAUDE.md`).

Con N=10 salen 12 aristas y diámetro 3.

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
