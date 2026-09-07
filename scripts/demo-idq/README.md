# demo-idq — QKC del proyecto sobre los KME reales de ID Quantique (UVigo)

Sustituye el simulador **quditto** por los nodos QKD reales del laboratorio de
la UVigo (KME de ID Quantique tras `castor.det.uvigo.es`). Dos QKC montan un
enlace `qkd` y cifran/descifran con OTP usando material real.

```
QKC-1 :7001 ──┐                        ┌── QKC-2 :7002
              ├── idq_shim :20010 ─────┤
   local:7101 ┘   (puente TLS, tonto)  └   local:7102
                        │
          enc → KME maestro (Alice) 192.168.100.102   (cert ETSIA)
          dec → KME esclavo (Bob)   192.168.100.107   (cert ETSIB)
```

## El QKC se configura solo

No hay que declarar **ningún** parámetro del KME. El QKC sondea `/status` al
primer relleno (`KmeClient::probe`) y se adapta a lo que anuncie el equipo:

| Dato | De dónde sale | Efecto |
|---|---|---|
| `key_size` | `/status` | adopta el tamaño del equipo aunque la config diga otro |
| `max_key_per_request` | `/status` | trocea el lote del `KeyStore` en las llamadas que admita |
| lote en `dec_keys` | probando | ETSI-014 no lo publica: al primer 400 baja a una por clave |

Contra el hardware IDQ (1 clave por petición, 256 bits) y contra quditto (128
por petición) funciona el mismo binario sin tocar nada.

**Lo único que sí hay que declarar es `sae_id`**, y no se puede evitar: el KME
no responde a nada sin él —devuelve `not managed SAE` incluso al `/status` si
el id no es uno de los aprovisionados— y ETSI-014 no tiene endpoint de
catálogo. Conocido uno, el `/status` da el del otro extremo.

## Por qué hay un puente y no se habla directo al KME

El puente **no traduce nada**, es un proxy transparente. Existe sólo porque el
QKC no puede abrir el TLS del equipo:

- Los certificados del IDQ son **ECDSA clásicos** y el QKC es **ML-DSA-only**
  (la carga de clave falla cerrada, sin caída silenciosa a clásico).
- El certificado de servidor del KME **no tiene SAN**, así que ninguna
  verificación de nombre puede pasar. `curl` lo salva con `-k`; el QKC, con
  buen criterio, no ofrece esa vía.

Resolverlo de verdad pide reemitir la PKI del KME (o un `kme_verify` explícito
de pinning); mientras tanto, el puente termina el TLS del equipo.

## Requisitos

- Acceso a los KME (túnel WireGuard a `192.168.100.102/107`, o
  `castor.det.uvigo.es:444/442`).
- Certs `ETSIA`/`ETSIB` (ChrisCA). El puente los busca en
  `trabajo_atlantic/nodos/certs`; ajusta `CERTS` en `idq_shim.py`.
- `cargo build --release -p qkc --bin qkc --bin qkc-test-client`.

## Uso

```bash
python3 scripts/demo-idq/idq_shim.py 20010 &
target/release/qkc --config scripts/demo-idq/qkc1.toml &
target/release/qkc --config scripts/demo-idq/qkc2.toml &
curl -fsS -XPOST 127.0.0.1:7201/forwarding-table -H 'content-type: application/json' -d '{"updates":{"2":2}}'
curl -fsS -XPOST 127.0.0.1:7202/forwarding-table -H 'content-type: application/json' -d '{"updates":{"1":1}}'
target/release/qkc-test-client listen --addr 127.0.0.1:7102 --count 1 --verbose &
target/release/qkc-test-client send --addr 127.0.0.1:7101 --dest 2 --message "hola"
```

## Verificado 2026-09-07

Con las configs de aquí, que no llevan un solo parámetro del KME:

```
kme.probe: ... adopto el suyo   configured=1024 announced=256
kme.dec_keys: el KME rechaza el lote; paso a una petición por clave
```

11/11 frames entregados byte-idénticos, `misses=0`, **0 respuestas 400**. Antes
del autodescubrimiento el QKC pedía `number=128` y `size=1024` a ciegas y el
equipo rechazaba **todos** los rellenos.

El IDQ es el cuello (buffer 100, ~4 claves/s): los `enc_refill` con 503 son
normales, se seca y el QKC reintenta. Sirve para validar corrección, no
rendimiento. **Parar los procesos al terminar**: el relleno drena el equipo sin
cesar.
