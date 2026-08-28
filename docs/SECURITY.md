# SECURITY.md — Hoja de ruta de autenticación y endurecimiento

Este documento es la **arquitectura de referencia** del trabajo de seguridad del
branch multi-host: qué hay que hacer, cómo, en qué orden y cómo verificarlo.
Se implementa fase a fase; cada fase es mergeable y testeable por sí sola.
Estado de cada fase se anota aquí mismo al completarla.

Referencia y parcialmente sustituye a `audit_2026_05.md` (findings H-5, H-6,
M-1) y `orr/TODO_SECURITY.md` (P2). Todo lo citado con `file:line` fue
verificado el 2026-08-27.

Estado global: **Fases 1, 2, 3, 5, 7 completas; 4 y 6 núcleo hecho**
(2026-08-27). Todas las fases recorridas. Quedan diferidos, con requisitos
claros: en 4, eliminar el socket de ACK (necesita testbed); en 6, pinning
estable (necesita persistir la identidad ORR — decisión del usuario) y auth
del caller de EstablishSecret (necesita testbed + separar puertos gRPC). Todo
lo demás implementado, opt-in y con default plaintext donde aplica, verificado
por tests unitarios (cripto, frames, identity-binding, cooldown, pinning).

> Trabajo en curso vía loop (cron `8213a816`, cada 20 min): completa las fases
> una a una verificando cada una; al terminar todas se borra el loop.

---

## 1. Alcance y modelo de amenaza

El branch apunta a despliegue multi-host: una institución = un `docker compose
up` con sus módulos, unidas por WAN. El adversario del modelo es alguien **con
acceso de red a los puertos expuestos entre instituciones** (pasivo: lee;
activo: inyecta/MITM). No modelamos compromiso del host de una institución.

### 1.1 Tabla de planos

Esta tabla es el artefacto central: cada fase apunta a una fila.

| Plano | Puerto (default) | ¿Cruza institución? | Auth hoy | Auth objetivo | Fase |
|---|---|---|---|---|---|
| DKMS↔SAE (ETSI-014) | `listen.sae_addr` :8443 | no (SAE local) — pero es el plano con mandato normativo | mTLS con 3 agujeros (§Fase 1) | mTLS real + autorización por SAE | 1, 2 |
| DKMS↔DKMS (ETSI-020) | `listen.peer_addr` :8444 | **sí** | mTLS real, pero identidad no comprobada y CA colapsada | mTLS + binding SAN==origen | 2, 4 |
| DKMS↔DKMS ACK socket | `peer_addr`+1000 (auto) | **sí** | **ninguna** (TCP plano, identidad = campo JSON) | eliminado — ACKs por ETSI-020 | 4 |
| todos↔SDN (HTTP admin) | SDN `http_addr` :8081 | **sí** | **ninguna** (decisión explícita, `sdn/src/http_api.rs:22-23`) | mTLS + identity binding | 3 |
| todos↔SDN (gRPC) | SDN `grpc_addr` :50053 | **sí** | ninguna (h2c) | TLS (CA de red) | 3 |
| QKC↔QKC (TCP binario) | `peer_listen` :20000 | **sí** | OTP + HMAC por frame (`frame_auth`), handshake HMAC/ML-DSA | hecho (5, 8) | 5, 8 |
| ORR↔ORR (capa cebolla) | dentro del payload QKC | **sí** | AES-256-GCM por capa + ventana anti-replay | hecho (8) | 8 |
| ORR↔ORR (gRPC bootstrap) | `peer_grpc_addrs` | **sí** | ninguna (pubkey sin firmar, overwrite anónimo) | pinning + challenge-response | 6 |
| DKMS↔ORR (gRPC) | `southbound.orr_endpoint` | no (mismo host) | ninguna | ninguna — red interna obligatoria (§1.2) | — |
| ORR↔QKC (gRPC) | interno | no | ninguna | ninguna — red interna obligatoria | — |
| QKC↔KME/quditto (ETSI-014) | `quditto_url` | no (KME propio) | ninguna | ninguna — red interna obligatoria | — |
| DKMS gRPC `DkmsControl` | `listen.grpc_addr` :50054 | no (operador) | **ninguna** (`Drain` borra buffers con 1 RPC) | bind localhost por defecto | 7 |
| /metrics (todos) | :9100–:9103 | no | ninguna | bind configurable | 7 |

### 1.2 No-objetivos explícitos — checklist de operador

La decisión de diseño es **auth fuerte solo donde cruza instituciones + el
plano SAE**. Los enlaces intra-institución quedan sin auth **a condición de**
que el operador cumpla esto (va también en `docker/README.md`):

- `DKMS↔ORR` transporta **material de clave en claro** en ese salto (el ORR lo
  cifra E2E hacia el ORR remoto, el salto local va desnudo:
  `dkms/src/southbound/orr.rs:173-200`). DKMS y ORR **deben** compartir host o
  red L2 confiable. Si algún día se separan en máquinas, ese enlace pasa a
  necesitar TLS — hoy no.
- Los puertos gRPC internos (ORR, QKC, `DkmsControl` :50054) y los `/metrics`
  **no se publican** fuera de la red compose/institucional.
- El cliente KME (`qkc/src/kme.rs:82-104`, reqwest sin TLS) habla con el KME
  **propio** de la institución; misma condición.

### 1.3 Anclas normativas

- **ETSI GS QKD 014**: exige TLS mutuo SAE↔KME. Es el mandato que hace de la
  Fase 1 la primera: es el único plano donde la auth no es opcional de diseño.
- **ETSI GS QKD 020** (draft): plano DKMS↔DKMS; asumimos mTLS entre KMS.

---

## 2. PKI objetivo

**Dos raíces offline por plano, generadas por script** (evolución de
`docker/gen-certs.sh`, que hoy crea una única `dkms-rust-ca`):

- **`net-ca`** — firma los certs de **nodos**: DKMS (planos peer y SAE-server),
  SDN, y los certs de cliente que los módulos presentan al SDN. Una por
  federación.
- **`sae-ca`** — firma **solo** certs de cliente de SAEs. En despliegue real,
  una por institución (cada DKMS configura en `sae_client_ca` la CA de *sus*
  SAEs); en los harness locales basta una compartida.

Por qué dos: hoy `sae_client_ca == peer_dkms_ca == ca.crt` en toda config
desplegada (`docker/render_config.py:222-226`), así que un cert de SAE
autentica como DKMS en el plano peer y viceversa — exactamente lo que
`dkms/src/config.rs:7-14` dice que no debe pasar. La separación es solo
generar dos pares en el script; el código ya tiene los dos campos.

**La CA es offline**: su clave vive con el operador y se usa al aprovisionar
(como hoy). **Descartado**: una CA online (p. ej. dentro del SDN o como sexto
módulo) — problema circular (el SDN es justo el componente contra el que
queremos autenticarnos; ¿y cómo autentica la CA a quien le pide un cert?) y
expone la clave raíz en un proceso conectado. Si la federación crece, el
upgrade path es `step-ca`/ACME como pieza externa, no código nuestro.

**Follow-on por institución** (Fase 7): `net-ca` puede fragmentarse en una
raíz por institución con *bundles* de confianza — el código ya soporta
bundles multi-PEM (`common/src/tls.rs::load_certs`,
`dkms/src/peer_client.rs::parse_ca_bundle` :148-166). Es madurez operativa,
no código.

Formatos SAN (ya parseados por `dkms/src/etsi_http/auth.rs`):
- Nodos: `URI:dkms://<node_id>`
- SAEs: `URI:urn:dkms:sae:<sae_id>` (el prefijo legacy `sae://` de demo-star
  se sigue aceptando: `auth.rs:307-326`)

---

## 3. Fases

Formato de cada fase: Objetivo / Cambios / Reutilizar / Compat y rollout /
Verificación / Tamaño.

### Fase 1 — Plano SAE: cerrar suplantación y añadir autorización

**Estado: IMPLEMENTADA (2026-08-27).** Solo código, sin cambios de infra.
Cambios efectivos:
- `dkms/src/etsi_http/auth.rs`: `resolve_peer_identity` ahora usa por defecto
  el cert verificado por rustls; el header `ssl-client-cert` solo se consulta
  con `AuthPolicy.trust_proxy_client_cert_header` (nuevo, default false). La
  decisión se aisló en `select_identity` (testeada).
- `dkms/src/etsi_http/mod.rs`: inyecta `AuthPolicy` en ambos routers desde
  `listen.trust_proxy_client_cert_header`.
- `dkms/src/config.rs`: nuevos `listen.trust_proxy_client_cert_header` y
  `sae.enforce_authorization` (default true).
- `dkms/src/service.rs`: `authorize_local_sae` (predicado puro
  `sae_served_locally`, testeado) al entrar en `status_for`/`handle_enc_keys`/
  `handle_dec_keys`; devuelve `UnknownSae` (404) si el SAE del cert no está en
  `sae_bindings` apuntando a este nodo.
- Tests: 4 nuevos unitarios, `cargo test -p dkms` 70/70, clippy limpio.
- Pendiente para cuando haya testbed: los negativos de integración (header
  forjado, SAE de otro DKMS) y activar el flag en los manifests k8s con
  nginx-ingress. `Forbidden` sigue sin sitio de construcción (reservado para
  el ACL de delegación, fuera de alcance de esta fase).

Notas de diseño originales (se mantienen como referencia):

**Objetivo.** Que el cert verificado por TLS sea la única fuente de identidad
y que solo los SAEs que este DKMS sirve obtengan claves.

Los tres agujeros actuales:
1. El header `ssl-client-cert` **gana** al cert verificado por rustls y su PEM
   nunca se valida contra ninguna CA (`dkms/src/etsi_http/auth.rs:289-297`,
   decode en `:77-86`). Cualquier SAE legítimo puede suplantar a cualquier
   otro con un header. Herencia de la era nginx-ingress.
2. Cero autorización: `DkmsError::{Unauthenticated, Forbidden, UnknownSae}`
   existen y están mapeados a HTTP (`dkms/src/error.rs:15,18,21`) pero tienen
   **cero sitios de construcción**. Nada comprueba que el SaeId del cert sea
   un SAE de este KME ni limita qué `slave_SAE_ID` puede pedir. El único check
   real del plano es `PendingStore::take_for_sae`
   (`dkms/src/state/pending.rs:105-121`) en `dec_keys`.
3. (Se cierra en Fase 2) CAs colapsadas.

**Cambios.**
- `dkms/src/etsi_http/auth.rs` — invertir `resolve_peer_identity`: la
  extensión `PeerIdentity` que inyecta `mtls.rs` (cert verificado) es la
  fuente por defecto; el header solo se consulta si el nuevo flag
  `listen.trust_proxy_client_cert_header = true` (default **false**,
  `#[serde(default)]` en `dkms/src/config.rs`). Documentar en el campo: solo
  es seguro si :8443 es alcanzable exclusivamente a través del proxy que
  termina el mTLS.
- Autorización en el extractor (o a la entrada de `service.rs`): el SaeId del
  cert debe estar en la lista local de SAEs (la misma que se anuncia al SDN
  vía `dkms/src/southbound/sdn_announce.rs:122-143`). Si no →
  `DkmsError::UnknownSae` (404 por ETSI) o `Forbidden` (403). Estrenar por fin
  esos variants. `take_for_sae` queda como defensa en profundidad.
- Aplicar el mismo criterio al plano ETSI-020 (usa el mismo
  `resolve_peer_identity`); el binding de identidad peer completo va en
  Fase 4.

**Reutilizar.** La extensión `PeerIdentity` de `dkms/src/etsi_http/mtls.rs`;
los strippers de prefijo SAN existentes (mantener `urn:dkms:sae:` **y**
`sae://`, demo-star usa el segundo).

**Compat.** Los despliegues k8s con nginx-ingress ponen
`trust_proxy_client_cert_header = true` en sus manifests (y solo ellos).
`render_config.py` no emite el flag → default seguro en docker/testbed.

**Verificación.** `tests/local-mesh/` keys smoke + testbed `t10` (camino feliz
intacto); nuevos negativos estilo T11 de `provision_certs.sh`:
(a) request con `ssl-client-cert` forjado de otro SAE → 401/403;
(b) cert válido de un SAE servido por *otro* DKMS → 403/404.

**Tamaño.** ~150–250 LOC + tests.

### Fase 2 — Split de PKI (atómica en aprovisionamiento)

**Estado: IMPLEMENTADA (2026-08-27).** Cambios efectivos:
- `docker/gen-certs.sh`: dos raíces `net-ca` (nodos) y `sae-ca` (SAEs) con
  `BasicConstraints CA:TRUE` explícito; helpers `ensure_ca`/`sign_with`.
- `docker/render_config.py`: emite `sae_client_ca=sae-ca.crt`,
  `peer_dkms_ca=net-ca.crt`, `control_plane_ca=net-ca.crt`.
- `common/src/tls.rs`: nuevo `client_config_mtls(ca,cert,key)` (helper cliente
  mTLS que la Fase 3 necesita) + `root_store` compartido con `client_config`.
- `dkms/config/default.toml`: nombres alineados (net-ca/sae-ca) + control_plane_ca.
- `scripts/demo-star/gen-tls.sh` + `dkms-template.toml`: split; 110 certs
  comprometidos regenerados.
- `tests/testbed/provision_certs.sh`: distribuye ambas CAs; retira claves de CA
  de las VMs; nota de test cross-plane.
- Harnesses cliente (demo-star, local-mesh, testbed) `--cacert ca.crt` →
  `net-ca.crt` (verifican el servidor DKMS, firmado por net-ca).
- Docs de operador (README, quick_start, node.dkms.yml, compose) actualizados.
- Verificado con openssl: nodo valida solo contra net-ca, SAE solo contra
  sae-ca, cross-plane rechazado en ambos sentidos; flujo gen-certs+render
  coherente end-to-end (cada fichero del TOML existe y valida). `common`
  compila, 23 tests verdes, clippy limpio.
- Pendiente para testbed real: el negativo cross-plane en vivo (t11 extendido)
  y los secrets k8s (gotcha H-5) cuando se toque ese despliegue.

Notas de diseño originales:

**Estado (histórico): pendiente.**

**Objetivo.** Materializar el §2: dos raíces, y dejar preparado el lado
cliente-mTLS que la Fase 3 necesita.

**Cambios.**
- `docker/gen-certs.sh`: generar `net-ca` y `sae-ca` (mismo flujo de reuso si
  ya existen); modo `--sae` firma con `sae-ca`, el resto con `net-ca`.
  **Las CAs con extensiones v3 y `BasicConstraints CA=true`** — sin eso el
  `WebPkiClientVerifier` construye un trust set vacío en silencio.
- `docker/render_config.py` (`render_dkms`, hoy `:222-226`): emitir
  `sae_client_ca = <certs>/sae-ca.crt`, `peer_dkms_ca = <certs>/net-ca.crt`,
  `control_plane_ca = <certs>/net-ca.crt` (este campo existe y está **sin
  leer**: `dkms/src/config.rs:110-113`; se lee en Fase 3).
- `common/src/tls.rs`: añadir `client_config_mtls(ca, cert, key)` con
  `.with_client_auth_cert(...)` — el `client_config` actual (`:73-87`, cero
  llamantes) solo hace `with_no_client_auth` y bloquea todo cliente mTLS.
- `tests/testbed/provision_certs.sh`: distribuir ambas CAs; extender el
  negativo T11 a **cross-plane** (cert de SAE presentado en :8444 → rechazo).
- k8s: los secrets de certs deben cubrir ambas CAs (gotcha H-5 del audit: el
  mTLS DKMS↔DKMS en k8s era no-funcional por certs self-signed por pod — la
  distribución debe ser explícita, no per-pod).

**Compat.** Rompe **material**, no código: hay que regenerar certs y
re-renderizar. Debe aterrizar **junto**: `gen-certs.sh` + `render_config.py`
+ `provision_certs.sh` + secrets k8s, o todos los harness fallan con errores
TLS confusos.

**Verificación.** `tests/local-mesh/mesh.sh` completo, testbed `t00` + `t10`,
negativo cross-plane nuevo.

**Tamaño.** ~300 LOC (shell/python en su mayoría).

### Fase 3 — Plano SDN: TLS en todo + registro con identidad

**Estado: 3a IMPLEMENTADA (2026-08-27); 3b pendiente.**

**3a — servidor + identity binding (hecho):**
- `sdn/src/config.rs`: `Option<SdnTlsCfg>` (cert/key/client_ca) + `http_ro_addr`.
  Sin `[tls]` → todo en claro (histórico; local-mesh/testbed intactos).
- `sdn/src/mtls.rs` (nuevo): loop mTLS + `PeerCertIdentity` (SAN del cert).
  Copia del patrón de `dkms/etsi_http/mtls.rs`; consolidar en `common` = follow-on.
- `sdn/src/http_api.rs`: `serve_with_tls` — rutas mutantes en mTLS (`http_addr`),
  read-only también en claro (`http_ro_addr`). Binding `identity_authorizes` /
  `require_identity` / `require_sae_owner` (puros, testeados) en
  `/register/{qkc,orr,dkms}`, `/sae`, `PUT /sae/{id}`: el SAN del cert debe
  casar el id anunciado / el `dkms_id` dueño. Sin cert (claro) se permite.
- `sdn/src/main.rs`: instala crypto provider rustls; pasa la config TLS.
- `sdn/Cargo.toml`: rustls/tokio-rustls/hyper-util/x509-parser + `rustls-tls`
  en reqwest.
- Verificado: `cargo test -p sdn` 131/131 (3 nuevos), clippy limpio, workspace ok.

**3b-i — announcer HTTP mTLS (IMPLEMENTADA 2026-08-27):**
- `common/src/http.rs` (nuevo): `announcer_client(url, tls, timeout)` — el
  esquema del URL decide (https→mTLS con `use_rustls_tls`+identity+roots,
  http→claro); `ClientTls`/`ControlTlsCfg` (owned, deserializable). Tests:
  scheme, plaintext-ok, https-sin-material-error.
- Announcers cableados: `dkms/southbound/sdn_announce.rs` + `sdn_http.rs`
  (usa `tls.control_plane_ca` con fallback a `peer_dkms_ca`);
  `orr/sdn_announce.rs` + `qkc/sdn_client.rs` (nueva sección `[tls]` opcional
  `Option<ControlTlsCfg>` en sus configs). `install_default` en orr/qkc main;
  `rustls` añadido a orr/qkc; `reqwest` con `rustls-tls` a common (feature
  unification lo propaga al workspace).
- `render_config.py`: `control_tls_lines` emite `[tls]` opcional para
  sdn(server)/orr/qkc(client) solo con `control_tls: true` (default plaintext);
  `http_ro_addr` para el SDN.
- Verificado: workspace compila; common 26 / dkms 70 / orr 61 / qkc 55 /
  sdn 131 tests verdes; clippy limpio; render condicional comprobado.

**3b-ii — gRPC TLS (IMPLEMENTADA 2026-08-27):**
- `sdn/src/grpc_server.rs`: `serve(svc, addr, tls)` — con `[tls]` monta
  `ServerTlsConfig` (identity net-ca + `client_ca_root`); sin él, plaintext.
- `dkms/src/main.rs`: `build_control_plane_tls` construye `ClientTlsConfig`
  (ca + identity) cuando `sdn_endpoint` es `https://`, pasado a
  `SdnClient::connect` (antes `None`). Los clientes gRPC QkcClient/OrrClient
  siguen en claro **a propósito**: DKMS↔QKC/ORR son intra-institución (§1.2).
- Verificado: dkms 70 / sdn 131 tests verdes, clippy limpio.

Notas de diseño originales:

**Estado (histórico): pendiente.** La mayor y la de más valor real: el plano announce
cruza la WAN, hoy va en claro, y **la respuesta del announce muta el peer
registry de cada módulo** (`dkms/src/southbound/sdn_announce.rs:174-185` →
`dkms/src/peers.rs:86-123`) — un POST anónimo o un MITM deciden a dónde se
envía material de clave. Además `PUT /sae/{id}` rebindea SAEs y
`/register/*` registra nodos, todo world-writable.

**Cambios — servidor (sdn).**
- `sdn/src/config.rs`: nueva sección `Option<SdnTlsCfg>` — `cert_path`,
  `key_path`, `client_ca` (= `net-ca`), y `http_ro_addr` para el listener
  read-only. Opcional: sin `[tls]` todo sigue plaintext (transición).
- Extraer el loop de servicio mTLS de `dkms/src/etsi_http/mtls.rs:37-109`
  (TcpListener → `TlsAcceptor` → hyper auto + extensión `PeerIdentity`) a
  `common/` (p. ej. `common/src/mtls_serve.rs`) y usarlo en
  `sdn/src/http_api.rs` en lugar del `axum::serve` plano (`:652-654`). Es la
  mayor pieza de reuso del plan; la Fase 4 la usa también.
- **Split de listeners**: rutas mutantes (`/register/*`, `POST/PUT/DELETE
  /sae…`, `/demand`, `/link-capacity`, `/paths`) en el listener mTLS; rutas
  read-only (`/healthz`, `/topology`, GETs) además en `http_ro_addr` plano —
  preserva la nota de diseño del web frontend (`http_api.rs:22-23`).
- **Identity binding** (la autorización de verdad, no solo el cifrado): en
  `/register/qkc|orr|dkms` y `PUT /sae/{id}`, el id del body debe coincidir
  con el SAN `dkms://<node_id>` del cert cliente. Un DKMS solo se anuncia a
  sí mismo y solo rebindea SUS SAEs.
- `sdn/src/grpc_server.rs:197-205`: `ServerTlsConfig` de tonic (el workspace
  ya compila tonic con `features = ["tls", "transport"]`).
- El reqwest del SDN que empuja forwarding tables a los QKCs
  (`sdn/src/service.rs:349`): **hoy no tiene backend TLS**
  (`default-features = false`) — añadir `rustls-tls` a `sdn/Cargo.toml`.

**Cambios — clientes (announcers y gRPC).**
- Announcers a HTTPS con el patrón que ya funciona:
  `use_rustls_tls() + Identity::from_pem(cert‖key) + add_root_certificate`
  (`dkms/src/peer_client.rs:76-96`). Tocar:
  `dkms/src/southbound/sdn_announce.rs:106-109`,
  `dkms/src/southbound/sdn_http.rs:52-55`,
  `orr/src/sdn_announce.rs:117-124`, `qkc/src/sdn_client.rs:176-183`.
  El reqwest de `orr` tampoco tiene backend TLS → `rustls-tls` en
  `orr/Cargo.toml`; el de `qkc` ya lo tiene.
- gRPC: usar por fin la plomería durmiente — `dkms/src/main.rs:144,243,271`
  pasan hoy `None` a `SdnClient/QkcClient/OrrClient::connect`; construir
  `Some(ClientTlsConfig)` desde `tls.control_plane_ca`. Para qkc/orr, cablear
  `common/src/ipc/grpc.rs::DialOpts::tls` (módulo entero sin usar).
- `rustls::crypto::aws_lc_rs::default_provider().install_default()` en los
  `main.rs` de sdn, orr y qkc — hoy solo lo hace `dkms/src/main.rs:73-74`, y
  sin él rustls hace panic al primer handshake.
- `render_config.py`: emitir `[tls]` para sdn/orr/qkc; `node.yml` de esos
  módulos gana `certs_dir`.

**Compat y rollout.** Dos pasos: (1) el SDN sirve TLS y plaintext a la vez
(config opcional), los módulos migran; (2) se retira el listener mutante
plaintext. Divisible en 3a (transporte TLS) y 3b (identity binding) si hace
falta.

**Verificación.** `mesh.sh` end-to-end (announce → registry → flujo de
claves); testbed `t30`/`t31` (add/remove node — son exactamente el escenario
announce); negativo nuevo: `PUT /sae/{id}` con cert cuyo SAN no es el DKMS
propietario → 403.

**Tamaño.** ~600–900 LOC en 4 crates + aprovisionamiento.

### Fase 4 — Plano peer DKMS: eliminar el ACK socket + binding en ext_keys

**Estado: NÚCLEO DE SEGURIDAD IMPLEMENTADO (2026-08-27); eliminación del
socket diferida a testbed.**
- **`ext_keys` ya está atado a la identidad del cert**: `handle_incoming_ext_keys`
  indexa `buffer_dec` por `peer` = `DkmsPeer.node_id` (SAN del cert), no por
  ningún campo del cuerpo. La Fase 1 (fin del header override) ya cerró la
  suplantación en este plano; el material sigue protegido además por la
  transport key OTP. No hacía falta un binding extra aquí.
- **`incarnation` endurecida** (`dkms/src/peers.rs`): `note_at` rate-limita los
  wipes a uno por `INCARNATION_WIPE_COOLDOWN` (30 s) por peer. La incarnation
  llega en un header de la entrega ORR sin autenticar; un peer que la cambiara
  en cada mensaje podía vaciar el buffer en bucle (DoS). Un reinicio legítimo
  la cambia una vez, así que el cooldown no lo estorba. Test:
  `wipe_is_rate_limited_within_cooldown`.
- **Ruta de ACK ETSI-020 (aditiva)**: `POST /kmapi/v1/ext_keys/ack` ahora llama
  `svc.handle_incoming_ack(peer.node_id, key_ids)` → `Generator::on_ack` con la
  identidad del **cert**, no un `from` autodeclarado como el socket TCP plano.
  Segura y disponible; hoy nadie la usa en salida.
- Verificado: dkms 71 tests verdes, clippy limpio.

**Diferido a una sesión de testbed (NO hacer a ciegas):** retirar el socket
TCP plano de ACK y migrar la **salida** a la ruta ETSI-020 mTLS
(`peer_client` → `peer_addr` del `PeerRegistry`, no al `ack_endpoint` del
header). `BatchedAckClient` tiene lógica de batching/paralelismo tuneada con
fixes documentados (spawn-not-await, flush paralelo) sobre el data path de
claves; reescribirla y borrar el socket cambia características de rendimiento
que **no se pueden verificar sin un despliegue multi-nodo**. Se hará con el
testbed (t20/t50) delante, con `use_ack_socket` on→off→delete.

**Estado (histórico): pendiente. Depende de Fase 3** (no autorizar contra un
`PeerRegistry` que aún sea escribible por un atacante vía announce).

**Objetivo.** Quitar los dos peores primitivos cross-institución: el ACK
socket TCP plano cuya identidad es el campo `from` del JSON
(`dkms/src/control/ack_socket.rs:99` → `generator.on_ack`), y la conexión
saliente a un `ack_endpoint` suministrado por el peer en un header
(`dkms/src/service.rs:963-975` — primitivo de conexión arbitraria). Y que el
plano :8444, que ya tiene mTLS, compruebe de verdad con quién habla.

**Cambios.**
- **ACKs → ETSI-020**: promover `POST /kmapi/v1/ext_keys/ack` (existe,
  hoy log-only: `dkms/src/etsi_http/v020.rs:44-58`) a llamar
  `generator.on_ack`, tomando la identidad del **cert** del peer
  (post-Fase 1/2), nunca del body. Salida: enviar ACKs por el
  `peer_client.rs` existente (ya mTLS) al `peer_addr` del `PeerRegistry` —
  no a ningún endpoint venido en headers.
- Borrar después: `dkms/src/control/ack_socket.rs` entero, la derivación
  `peer_addr+1000` (`dkms/src/main.rs:318-322`), el header `ack_endpoint` y
  su chequeo de arranque (`generator.rs::check_ack_endpoint`).
- **Binding en `handle_incoming_ext_keys`** (`dkms/src/service.rs:605`): el
  origen declarado en el body == SAN del cert, y debe existir en
  `PeerRegistry`; si no → `Unauthenticated`/`UnknownSae`. Hoy indexa el pool
  con cualquier string que llegue.
- **`incarnation` endurecida**: hoy un header forjado en el stream ORR puede
  borrar los buffers locales de un peer (`service.rs:886-888`). Validar que
  la incarnation solo se acepta de peers conocidos y tratar transiciones como
  monótonas por proceso (primera vista ≠ restart, como ya documenta
  CLAUDE.md). La auth completa del salto ORR queda fuera de alcance (§1.2),
  pero este trigger es accionable por el extremo remoto del camino → se
  valida.
- Documentar en el doc del plano: el `key_digest` es SHA-256 **sin clave** —
  detecta corrupción, no autentica; la protección interna real es la
  transport key OTP.

**Compat.** Una release corre ambos caminos de ACK: flag `use_ack_socket`
(default on → default off → borrar). Encaja con rollout mixto en testbed.

**Verificación.** `tests/local-mesh/stress.sh` (regresión de throughput de
ACKs), testbed `t20` (load) y `t50` (idle/reconnect); negativos: `ext_keys`
con cert/origen que no casan → 403; ACK de peer equivocado → ignorado.

**Tamaño.** ~400 LOC **netas negativas** (borra un transporte entero).

### Fase 5 — QKC↔QKC: handshake PQC autenticado (HMAC-PSK)

**Estado: IMPLEMENTADA (2026-08-27), opt-in.** Sin `link_psk`/`pqc_auth=off`
el path es byte-idéntico al actual (frames 0x21/0x22). Cambios:
- `common/src/crypto/link_mac.rs` (nuevo): HMAC-SHA256 con tags de dominio
  (`QKCINIT`/`QKCRESP`/`QKCNOTIFY`), lp16, verify constant-time. MAC sobre
  `tag ‖ epoch ‖ sender_id ‖ receiver_id ‖ lp16(blob) ‖ lp16(suite) ‖ key_size`
  — ata la identidad (u32 sin verificar en el wire) y cierra el mismatch
  silencioso de suite/key_size. 5 tests. `hmac` añadido a common.
- `wire/src/lib.rs`: frames `0x23` INIT_AUTH, `0x24` RESP_AUTH, `0x25`
  NOTIFY_AUTH (payload viejo ‖ tag 32 B); `0x26`/`0x27` reservados para ML-DSA.
  No cambia encode/decode (kind es genérico).
- `qkc/src/config.rs`: `LinkConfig.link_psk` (base64) + `pqc_auth` (enum
  `off|prefer|require`, default off). PSK solo local.
- `qkc/src/pqc_handshake.rs`: `send_handshake` emite el frame AUTH con tag si
  hay PSK y modo≠off; `accept` verifica el MAC antes de tocar el secreto y en
  `require` descarta handshakes en claro. Mitiga el ataque documentado
  (INIT forjado que sobrescribía el secreto vivo). Tests: round-trip
  autenticado, `require` rechaza MAC inválido y plaintext.
- `qkc/src/transport/peer_server.rs`: enruta 0x23/0x24 → `handle_pqc(authed)`.
- `docker/render_config.py`: emite `link_psk`/`pqc_auth` por enlace si se declaran.
- Verificado: common 31 / qkc 57 / wire 12 tests, clippy limpio.
- Pendiente dentro de 5: cablear el `FRAME_KEY_IDS_NOTIFY_AUTH` (0x25) en el
  path de notify (resync). Es un trigger, no la vía de sobreescritura de
  secreto; vive en enlaces QKD (otro tipo). Menor prioridad; frame reservado.
- **Upgrade ML-DSA IMPLEMENTADO (2026-08-27)** — firma post-cuántica real
  (frames 0x26/0x27), primer paso hacia "auth PQC de verdad":
  - `common/src/crypto/pqc_sign.rs` (nuevo): ML-DSA-65 (FIPS 204, crate
    `ml-dsa`) keygen/sign/verify + `sign_handshake`/`verify_handshake` sobre
    el mismo mensaje canónico que el HMAC (ata época/ids/blob/suite/key_size).
    Seed sk 32 B (Zeroizing), vk 1952 B, firma 3309 B. 5 tests.
  - `qkc/src/config.rs`: nuevo modo `pqc_auth = sign`; `sign_secret_seed`
    (nodo, base64) + `peer_verify_key` por enlace (base64). Solo se comparten
    claves **públicas** (mejor que el PSK simétrico).
  - `qkc/src/pqc_handshake.rs`: enum `RecvAuth {Plain,Hmac,Signed}`;
    `send_handshake` firma con ML-DSA en modo `sign` (frames 0x26/0x27);
    `accept` verifica la firma antes de tocar el secreto y en `sign` descarta
    frames en claro/HMAC. Test `signed_handshake_round_trip_and_rejects`
    (round-trip + rechazo de firma con clave equivocada y de frame en claro).
  - `qkc/src/transport/peer_server.rs`: enruta 0x26/0x27 → `RecvAuth::Signed`.
  - `render_config.py`: emite `sign_secret_seed` (nodo) y `peer_verify_key`.
  - Verificado: common 36 / qkc 58 tests, clippy limpio. Opt-in (default off).
  - **Bootstrap ORR firmado con ML-DSA — HECHO (2026-08-27)**: el ORR firma su
    pubkey ML-KEM (efímera) con una identidad de firma **estable** en config
    (`sign_secret_seed`), y el peer la verifica con `peer_verify_keys[orr_id]`.
    Cierra el MITM sobre `GetPublicKey` sin persistir la identidad ML-KEM.
    Proto: `GetPublicKeyResponse.signature`. Código: `common pqc_sign::
    {sign,verify}_orr_pubkey`, `OrrIdentity::sign_pubkey_announcement`,
    `PeerRegistry::verify_announcement` (`SigVerdict`), `bootstrap.rs` verifica
    en fetch y en el refresh del rebootstrap pasivo. common 37 / orr 63 tests.
  - **Certificados TLS con firma ML-DSA** (SAE/DKMS/SDN) — NÚCLEO HECHO Y
    PROBADO (2026-08-27). `common/src/tls_pqc.rs`: `pqc_crypto_provider()` =
    provider aws-lc-rs + verificación ML_DSA_65 + `PqcKeyProvider` que carga
    claves ML-DSA (crate `ml-dsa`) con fallback a RSA/ECDSA/EdDSA. **Probado con
    un handshake mTLS completo in-memory donde CA, cert de servidor y cert de
    cliente son todos ML-DSA** (test `full_mtls_handshake_with_ml_dsa_certs`).
    common 39 tests, clippy limpio. Interop clave: la clave ML-DSA se genera en
    forma seed-only (`openssl genpkey -provparam ml-dsa.output_formats=seed-only`),
    y webpki necesita el feature `aws-lc-rs-unstable`.
    **CABLEADO Y COMPLETO (2026-08-27)**: `common::tls::{server_config,
    client_config,client_config_mtls}` usan `pqc_crypto_provider()`
    (retrocompatible: RSA/ECDSA siguen funcionando, y además ML-DSA).
    `gen-certs.sh KEY_ALG=ml-dsa-65` emite certs ML-DSA (claves seed-only).
    Verificado: `public_api_loads_ml_dsa_certs` (la API pública carga certs
    ML-DSA) + el handshake mTLS completo. gen-certs produce certs con
    `Signature Algorithm: ML-DSA-65` (CA/nodo/SAE), cadena válida.

**RESULTADO: las TRES firmas de autenticación soportan PQC (ML-DSA):**
QKC handshake ✅, bootstrap ORR ✅, certs TLS ✅. Todo **opt-in**
(retrocompatible), verificado por tests. Falta solo una **decisión de
despliegue**: flipear los defaults a PQC-por-defecto (`KEY_ALG=ml-dsa-65`,
`pqc_auth=sign`, seeds ML-DSA en config) — es un cambio *breaking* que obliga
a regenerar todos los certs y configurar las claves de firma, y a re-verificar
con el testbed; la capacidad está lista, el flip es del operador.

Notas de diseño originales:

**Estado (histórico): pendiente.**

**Objetivo.** Parar el ataque activo verificado en la exploración: un
`FRAME_PQC_KEM_INIT` sin autenticar con el mismo epoch y otra pubkey hace que
el responder re-encapsule y **sobrescriba el secreto vivo del epoch**
(`qkc/src/pqc_handshake.rs:381-389`, `publish_replacing`). Además un
`FRAME_KEY_IDS_NOTIFY` forjado con epoch alto en el prefijo del key_id
dispara resyncs/relinks arbitrarios (`qkc/src/pqc_source.rs:489-491,162-165`).
La identidad del peer en este plano es hoy `frame.sender_id` (u32) sin
verificación alguna (`qkc/src/transport/peer_server.rs:133-134`).

**Mecanismo: HMAC-SHA256 con PSK por enlace** (ver Decisions Log §4.4 para
el porqué frente a ML-DSA).

**Cambios.**
- Nuevo `common/src/crypto/link_mac.rs` generalizando `orr/src/macs.rs`
  (tags de dominio, encoding `lp16` length-prefixed, verify constant-time —
  patrón ya probado en las RPCs de rotación del ORR).
- Entrada del MAC:
  `TAG("QKCINIT"|"QKCRESP"|"QKCNOTIFY") ‖ epoch_be ‖ lp16(sender_id) ‖
  lp16(receiver_id) ‖ lp16(blob) ‖ lp16(suite_id) ‖ key_size_bits_be`.
  Atar sender/receiver cierra la identidad; atar `suite_id`/`key_size_bits`
  cierra el mismatch silencioso actual (ambos son config-only y nunca van en
  el wire — dos extremos mal configurados derivan material de longitud
  distinta sin que nada lo detecte).
- Frames nuevos en `wire/src/lib.rs`: `FRAME_PQC_KEM_INIT_AUTH = 0x23`,
  `FRAME_PQC_KEM_RESP_AUTH = 0x24`, `FRAME_KEY_IDS_NOTIFY_AUTH = 0x25`
  (payload = payload viejo ‖ tag 32 B). **No** tocar 0x21/0x22 (romperia el
  parse de blob de tamaño fijo de `split_epoch`) y **no** subir el byte de
  versión del MAGIC (hard-break deliberado, reservado para incompatibilidades
  reales). Reservar 0x26/0x27 para el upgrade ML-DSA.
- Config: `link_psk` (base64) en `LinkConfig` de `qkc/src/config.rs` y en el
  `node.yml` (`render_qkc`). **El PSK es solo config local: el SDN no
  transporta secretos** — el announce/peer-set sigue igual; un enlace creado
  por el SDN sin PSK local funciona en modo `off` y se registra un warn.
- `qkc/src/pqc_handshake.rs`: `send` (`:161-171`) emite 0x23/0x24 si el
  enlace tiene PSK; `handle_init`/`handle_resp` verifican tag **antes** de
  cualquier `publish_replacing`. Modo por enlace
  `pqc_auth = off | prefer | require`.
- Mitigación barata incluso en `off`: un re-INIT del mismo epoch con pubkey
  distinta se ignora (log warn) salvo tag válido — el relink legítimo siempre
  negocia epochs **nuevos** por encima de ambas ventanas (invariante ya
  documentado en CLAUDE.md), así que rechazar el replace del mismo epoch no
  rompe la recuperación.

**Compat y rollout.** Los peers viejos **ignoran en silencio** los frame
kinds desconocidos (`peer_server.rs:100-102`) y no hay handshake de versión
en la conexión → el descubrimiento es solo por timeout. El flag tri-estado
no es pulido opcional, **es el mecanismo de rollout**: desplegar código en
`off`/`prefer` en toda la malla, repartir PSKs, subir a `require`. `prefer`
manda 0x23 y cae a 0x21 tras timeout acotado (log); `require` nunca cae.

**Verificación.** testbed `t40`/`t41` (restart PQC / restart storm — son
exactamente este camino), `mesh.sh` en modo mixto `off`/`prefer`; unit tests
junto a los existentes de `pqc_handshake.rs`; negativo: INIT forjado sin tag
o con tag inválido bajo `require` → epoch intacto.

**Tamaño.** ~400–500 LOC.

### Fase 6 — ORR↔ORR: bootstrap con pinning + challenge (ejecuta TODO_SECURITY P2)

**Estado: NÚCLEO HECHO (2026-08-27); pinning estable + auth del caller
BLOQUEADOS por decisiones de diseño.**

Implementado (opt-in, tofu default = comportamiento actual):
- `orr/src/config.rs`: `bootstrap_trust = tofu | strict` (default tofu).
- `orr/src/peers.rs`: snapshot inmutable de los pins de `peer_pubkeys` +
  `verify_fetched_pubkey` (puro, testeado) → `Accept | AcceptPinMismatch |
  Reject`; `with_pubkeys_trust`.
- `orr/src/bootstrap.rs`: `fetch_pubkey` contrasta la pubkey anunciada contra
  el pin: en tofu avisa si difiere (señal de MITM o reinicio del peer), en
  strict la rechaza si no casa un pin.
- Verificado: orr 62 tests, clippy limpio.

**BLOQUEADO — requiere decisión del usuario (NO implementado a ciegas):**
1. **Pinning estable / strict práctico** exige **persistir la identidad ML-KEM
   del ORR en disco**. Hoy se **regenera en cada arranque** (`service.rs`:
   "regenerada cada vez que arranca el proceso — TODO persistir"), así que la
   pubkey cambia en cada reinicio y un pin de config queda obsoleto → strict
   rompería la recuperación tras reinicio. Persistir la identidad **roza la
   preferencia registrada "material de clave en RAM, no en disco"** — aunque
   una clave de identidad long-term es distinta de los buffers de sesión.
2. **Auth del caller de `EstablishSecret`** (cerrar el overwrite anónimo del
   `bootstrap_secret`): mTLS ORR↔ORR o challenge-response mutuo. El gRPC del
   ORR sirve **en el mismo puerto** el tráfico intra-institución (DKMS→ORR,
   §1.2 sin auth) y el cross-institución (ORR→ORR bootstrap); ponerle mTLS
   rompería el primero sin separar puertos. El challenge (patrón `macs.rs`,
   como las RPCs de rotación ya HMAC-authed) es un cambio de protocolo no
   verificable en local.

Notas de diseño originales:

**Estado (histórico): pendiente.**

**Objetivo.** Hoy `GetPublicKey` devuelve la pubkey ML-KEM **sin firmar** y el
único check del cliente es comparar el `orr_id` auto-declarado
(`orr/src/bootstrap.rs:258-267,305-321`); y `EstablishSecret` permite a
cualquier caller **sobrescribir** `bootstrap_secret[from]`/`master_secret`
(`orr/src/grpc_server.rs:148-217`, sin idempotencia por diseño). Todo sobre
gRPC plano que cruza instituciones.

**Cambios.**
- El mapa `peer_pubkeys` del TOML (**ya existe**: `orr/src/config.rs:84-91`,
  con prioridad sobre la pubkey fetcheada) pasa a ser ancla obligatoria bajo
  `bootstrap_trust = tofu | strict` (default `tofu` por compat; `strict`
  recomendado cross-institución en el doc de despliegue). En `strict`,
  `GetPublicKey` solo confirma; la confianza viene del TOML.
- Challenge-response mutuo en `EstablishSecret`, MACeado con el bootstrap
  secret recién derivado, con el patrón de tags de `orr/src/macs.rs` — las
  RPCs de rotación **ya** van HMAC-authed así
  (`orr/src/rotation.rs:320,357,411`); el bootstrap es la única RPC del
  servicio que queda sin autenticar.
- Rechazar `EstablishSecret` que sobrescriba un `from` ya establecido salvo
  que pase el challenge — mata el overwrite anónimo conservando el
  re-bootstrap legítimo.
- El transporte gRPC ORR↔ORR gana el TLS de control de la Fase 3 (misma
  plomería tonic + `net-ca`), puesto que cruza instituciones (fila §1.1).

**Verificación.** Unit tests de `orr` + `tests/local-mesh/bootstrap_times.py`
(regresión de latencia de bootstrap); negativo: `EstablishSecret` rogue contra
peer establecido → rechazado.

**Tamaño.** ~250–350 LOC.

### Fase 7 — Endurecimiento residual y deuda

**Estado: IMPLEMENTADA (2026-08-27).**
- **Zeroize (audit M-1)**: `OrrIdentity.secret_key` pasa a `Zeroizing<Vec<u8>>`
  (`orr/src/identity.rs`) — era el único secreto long-lived sin envolver; el
  resto (qkc `pending_sk`, orr `ephemeral_sks`/`bootstrap`/`master` secrets) ya
  iba en `Zeroizing`. No se pone `Drop` en `KemKeypair`/`KemEncap` porque
  impediría mover sus campos públicos (`public`/`ciphertext`); nota M-1 añadida.
- **`DkmsControl` (Drain) a localhost por defecto**: `dkms/config/default.toml`
  `grpc_addr = 127.0.0.1:50054` con comentario (el RPC `Drain` borra buffers).
  Los despliegues que lo necesiten remoto lo abren y firewalean.
- **`/metrics`**: comentario de que va sin auth y responde a cualquier path.
- **Checklist de operador**: sección "Seguridad y firewall" en `docker/README.md`
  (qué puertos deben quedar en red interna; DKMS↔ORR lleva material en claro).
- Verificado: common 31 / orr 62 / dkms 71 tests, clippy limpio, workspace compila.
- Follow-on documentado (no bloqueante): CA por institución con bundles (el
  código ya soporta bundles multi-PEM); cierre formal de H-5/H-6 del audit.

Notas de diseño originales:

- Zeroize de `KemKeypair`/`KemEncap` (audit M-1, `common/src/crypto/pqc.rs`).
- `DkmsControl` :50054: bind por defecto a localhost (config), documentado
  como plano de operador intra-host — `Drain` borra pending + buffers con un
  RPC (`dkms/src/grpc_server.rs:100-119`).
- `/metrics`: bind configurable (y nota: el responder contesta a cualquier
  request ignorando el path, `common/src/metrics.rs:46-81`).
- Bundles per-institution para `net-ca` (§2).
- Checklist de operador §1.2 copiada a `docker/README.md`.
- Cierre documental: notas H-5/H-6 en `audit_2026_05.md`; corregir
  `dkms/README.md:5` (dice ":8080, mTLS optional" — ambos falsos) y
  `docs/ipc.md` (apunta a `common/src/ipc/binary_tcp.rs`, que no existe; el
  wire real es `wire/src/lib.rs`, MAGIC v3).

**Tamaño.** ~200 LOC + docs.

---

### Fase 8 — Integridad, origen de datos y frescura del plano de datos (HECHA 2026-08-28)

Las fases 1-7 son **autenticación de entidad**: quién habla. Esta es la otra
mitad, y se resuelve distinto: que **cada mensaje** venga de quien dice, no haya
sido modificado, y no sea uno de ayer reinyectado hoy. Autenticar la conexión no
basta si luego los frames van sin MAC.

**De qué se partía.** El payload del enlace QKC↔QKC va cifrado con OTP y las
capas del ORR iban con `plaintext ⊕ HKDF(master_secret, key_id, len)`. Los dos
son maleables: quien pudiera tocar el ciphertext podía aplicarle un delta
arbitrario y el receptor descifraba un plaintext modificado sin enterarse. Lo
único que había enfrente era el `key_digest` del DKMS, un SHA-256 **sin clave**
que funciona sólo porque viaja dentro del cifrado —integridad apoyada en la
confidencialidad— y que además sólo cubre el camino del `DKMS_BUFFER`. Y
`frame.sender_id` era un `u32` en claro que nadie comprobaba.

**Dos capas, porque protegen cosas distintas.**

1. **Salto a salto, en el enlace** (`common/src/crypto/frame_mac.rs`,
   `qkc/src/frame_auth.rs`). HMAC-SHA256 sobre el frame entero —identidades,
   `key_ids` con longitud explícita, los dos headers que el QKC propaga sin
   mirar, y el ciphertext—, con clave `HKDF(link_psk, salt = session)`. Kinds
   `0x04`/`0x05` y `0x25`; el `PAYLOAD` acaba en `session ‖ counter ‖ tag`.
   Política por enlace: `frame_auth = off | prefer | require`.
2. **Extremo a extremo, en la cebolla** (`orr/src/onion.rs`). AES-256-GCM por
   capa con `key_id ‖ epoch_id ‖ max_hops ‖ session ‖ counter` como AAD. Hace
   falta *además* del MAC de enlace porque éste es salto a salto: el QKC
   descifra, recifra y **recalcula el MAC**, así que un QKC del camino podría
   alterar la carga sin que nada lo notase. El tag AEAD lleva la clave del par
   de ORRs, que el QKC no tiene.

**La frescura va dentro del mensaje autenticado, por diseño.** Un MAC que sólo
cubre el contenido deja el frame repetido igual de válido. En las dos capas la
pareja es `(session, counter)`: `session` es una encarnación aleatoria por
arranque de proceso —sin ella, un emisor que reinicia vuelve al contador 1 y sus
mensajes legítimos son indistinguibles de un replay— y `counter` un monotónico.
El receptor lleva una ventana deslizante (1024) y un conjunto de sesiones
retiradas. En la cebolla, `(session, counter)` los pone el ORR de **origen** y
entran en el AAD de todas las capas: el que reenvía tiene que copiarlos tal
cual, porque si los cambia el peel del salto siguiente falla.

**Tres invariantes que no hay que romper.**

- **Verificar el MAC ANTES de tocar la ventana**, en las dos capas. Al revés,
  cualquiera tira la ventana del receptor mandando basura con una `session`
  inventada; con el MAC delante hace falta la clave para siquiera proponer una
  sesión nueva.
- **Un enlace con MAC se declara en los DOS extremos**, igual que `pqc_auth =
  sign`: la raíz es `link_psk`, config local que la SDN no transporta ni debe,
  así que el extremo que recibe el enlace por anuncio se queda sin PSK y
  descarta todo. `render_config.py` emite `link_psk`/`frame_auth` **fuera** de
  la bifurcación qkd/pqc — estuvieron dentro de la rama pqc y un enlace QKD los
  perdía al renderizar, en silencio.
- **La raíz es simétrica y de 256 bits**, así que es quantum-safe (Grover deja
  128 efectivos) y no hace falta firma por frame: una ML-DSA son 3309 B por
  mensaje, inviable a este ritmo. Upgrade documentado: derivar la raíz del
  secreto ML-KEM del enlace, o hacer *key growing* con bits QKD.

**Diagnóstico.** Línea `qkc.frame_auth` cada 5 s por enlace con
`signed/verified/bad_mac/replayed/plain_ok/plain_rej`; en un enlace sano lo que
un extremo firma coincide con lo que el otro verifica y los cuatro últimos son
0. En el ORR, `replay_dropped` en `orr.state`, separado de `peel_failed`: uno es
"tag válido, mensaje repetido" y el otro "tag que no cuadra".

**Verificación.** 448 tests unitarios, más dos pruebas de integración que
comprueban lo que los unitarios no pueden:

- `tests/local-mesh/frame_auth_negative.sh` — a un nodo se le quita la PSK en
  caliente y se comprueba que sus vecinos lo dejan **fuera** (`plain_rej` ~1700
  en cada uno) en vez de degradar el enlace a no autenticado: el tráfico con él
  se congela. El primer rechazo es el NOTIFY sin firmar, que es fail-closed en
  cuanto el receptor tiene PSK.
- `tests/local-mesh/frame_auth_tamper.sh` + `frame_tamper.py` — un proxy TCP
  entre dos QKC, **sin ninguna clave**, voltea un bit cada N frames. Resultado:
  1204 frames, 60 tocados, `bad_mac = 60` en el peer y `recv_corrupt = 0` en el
  DKMS. Ni una modificación se cuela, y ninguna llega al material. El proxy
  parsea el wire de verdad, para que lo que falle sea el MAC y no el parser.

**Coste, medido en CESGA sobre enlaces QKD (`ringchords`, certificados
ML-DSA-65).** A N=10, contra el mismo despliegue sin MAC de frame: `media`
5300.2 → 5298.2 claves/s (−0.04 %), `mucha` 13146.3 → 13147.3 (+0.01 %). A
**N=30**, contra `rsa-qkd-authoff` —o sea, contando también el salto de
certificados RSA a ML-DSA—: `media` 4432.9 → 4429.6 y `mucha` 8766.6 → 8753.8,
**−0.1 % en ambos**, con p95 2.2 → 2.2 y 7.8 → 8.0 ms.

**Corrección.** En las 15 celdas de las tres campañas: `bad_mac`, `replayed`,
`plain_ok`, `plain_rej`, `orr_ko` y `corrupt` **a cero**, y ni un solo
handshake TLS fallido. A N=30 se verificaron 7.9 millones de frames. La prueba
funcional más dura es el régimen `poca`, que compara los bytes de los dos
extremos de cada intercambio ETSI-014: **870/870 parejas de la malla completa
con buffer lleno y 3180 intercambios 100 % byte-idénticos**.

**Una trampa que costó una campaña entera y conviene no repetir.** La primera
versión metía `nonce ‖ ct ‖ tag` dentro del payload de la cebolla y midió
**−51 %**. El OTP del enlace trocea en bloques de `key_size_bits / 8` y gasta
una clave QKD por bloque, así que 28 bytes de más convierten un mensaje de 32 B
en dos bloques: el doble de material por salto. Es invisible en enlaces PQC.
Ver la gotcha del troceado en CLAUDE.md.

**Lo que sigue sin cubrir.**

- **Disponibilidad.** Un QKC intermedio malicioso ya no puede alterar la carga
  —lo impide el tag AEAD de la cebolla— pero sigue viendo el `header_orr_mp` en
  claro y puede descartar frames. No es objetivo de esta fase.
- **La ventana anti-replay es RAM-only, así que un reinicio la vacía.** Justo
  después de reiniciar, un receptor acepta el primer mensaje que le llegue sea
  cual sea su contador, y sólo a partir de ahí empieza a filtrar: un mensaje
  reciente capturado antes del reinicio puede colarse en esa rendija. Es la
  misma propiedad que tiene cualquier anti-replay sin estado persistente (IPsec
  al rotar SA). Persistirla exigiría meter estado en disco en un sistema
  diseñado sin él, y el ataque necesita capturar y reinyectar en el hueco de un
  reinicio concreto.
- **`default_max_hops = 0` (passthrough) apaga la protección extremo a extremo**
  sin decir nada. En ese modo no hay capa de cebolla: el payload va en claro
  hasta el QKC, que lo cifra con el OTP del enlace, y el frame ni siquiera pasa
  por `handle_onion_in`, así que no hay tag AEAD ni ventana anti-replay — sólo
  queda el MAC de enlace, salto a salto. El default es `1` en el Rust y en
  `render_config.py`, así que hay que ponerlo a mano para perderlo; pero si
  alguien lo hace, que sepa lo que apaga.
- **El socket de ACK del DKMS sigue sin autenticar** (`ack_socket.rs`, Fase 4):
  acepta TCP plano de cualquiera y saca el `from` del cuerpo. No compromete
  material —es contabilidad del generador— pero sí es autenticación de origen
  que falta, en un plano que cruza instituciones.
- **Las claves de sesión SAE** ya llevan `session_key_digest` (una huella ligada
  al `key_id`, comprobada antes de guardar y de acusar recibo), así que el caso
  de "los dos SAE se llevan claves distintas en silencio" está cerrado. Lo que
  no hay es una comprobación que el propio SAE pueda hacer: se fía de su KME.

## 4. Decisions log

**4.1 PKI: dos raíces offline por plano (elegido) vs una CA única vs CA
online como módulo.** La única CA actual colapsa los dos planos (un SAE puede
autenticarse como DKMS). La CA online se descartó por el problema circular de
enrolment (¿cómo autentica a quien le pide el cert?) y por exponer la clave
raíz en un servicio de red; queda anotada como evolución futura con
`step-ca`/ACME externo si la federación escala.

**4.2 SDN: mTLS + identity binding (elegido) vs TLS+bearer token vs solo TLS
de servidor.** El peligro del SDN no es solo el acceso: es que
`/register/*` y `PUT /sae/{id}` reescriben registries que enrutan material.
Eso necesita **identidad** ("este DKMS solo rebindea sus SAEs"), que un token
simétrico compartido no puede expresar; el SAN del cert la da gratis y
reutiliza la PKI de la Fase 2. Solo-TLS cifra el canal pero deja el registro
abierto. La fricción con el web frontend se resuelve con el listener
read-only plano.

**4.3 ACK: migrar a ETSI-020 (elegido) vs TLS sobre el socket.** La ruta ya
existe, el cliente mTLS ya existe, y la migración borra un transporte ad-hoc
entero + el puerto auto-derivado + el primitivo de conexión saliente
arbitraria. TLS sobre el socket endurecería algo que no debería existir y
violaría la convención del repo (gRPC o TCP binario, no un tercero).

**4.4 Handshake PQC: HMAC-PSK ahora, ML-DSA como upgrade (elegido) vs ML-DSA
directo vs ed25519.** PSK: cero dependencias nuevas (`hmac`+`sha2` en el
workspace, patrón `orr/src/macs.rs` probado y constant-time), tag de 32 B vs
firma de ~3.3 KB, y encaja con el modelo de confianza real del QKC — que hoy
solo conoce `neighbor_id + addr` y no tiene ninguna infraestructura de
distribución de pubkeys. HMAC es simétrico → sigue siendo seguro frente a
adversario cuántico. ML-DSA (crate RustCrypto pre-1.0) queda como upgrade
path con los frames 0x26/0x27 reservados para cuando exista distribución de
pubkeys de firma; ed25519-vía-ring descartado por romper la coherencia
post-cuántica del proyecto sin ventaja operativa sobre el PSK.

**4.5 Gaps de backend TLS en reqwest: arreglar en Fase 3.** `sdn` y `orr`
declaran reqwest con `default-features = false` sin feature TLS — cualquier
URL `https://` falla en runtime y bloquearía el rollout en silencio.

---

## 5. Gotchas de implementación (leer antes de cada fase)

1. **Crypto provider rustls**: solo `dkms/src/main.rs:73-74` llama
   `install_default()`. Todo binario que gane rustls (sdn/orr/qkc, Fase 3)
   hace panic al primer handshake sin esa línea.
2. **`use_preconfigured_tls` de reqwest: NUNCA.** Falla con "Unknown TLS
   backend" por mismatch de versión rustls (aviso ya codificado en
   `dkms/src/peer_client.rs:7-11`). El patrón que funciona:
   `use_rustls_tls() + Identity::from_pem + add_root_certificate`.
3. **config-rs nested-merge**: no prometer overrides por env de las secciones
   `[tls]` nuevas (setear un campo reemplaza la sección entera). El camino es
   render por fichero (`render_config.py`).
4. **Frames desconocidos se ignoran en silencio** y no hay handshake de
   versión en la conexión QKC↔QKC → el rollout de la Fase 5 es solo viable
   con el flag `off|prefer|require`. No subir el MAGIC.
5. **Fase 2 es atómica en aprovisionamiento**: gen-certs + render_config +
   provision_certs + secrets k8s a la vez, o todos los harness se ponen rojos
   con errores TLS confusos.
6. **ALPN**: todo servidor TLS nuevo anuncia `h2` **y** `http/1.1` o los
   clientes reqwest con `http2_prior_knowledge` mueren con
   `NoApplicationProtocol` (ya documentado en `common/src/tls.rs:62-68`; el
   loop extraído a `common/` lo trae de serie).
7. **`WebPkiClientVerifier`** necesita CAs con `BasicConstraints CA=true` y
   extensiones v3, o construye un trust set vacío en silencio.
8. **Mantener ambos prefijos SAN de SAE** (`urn:dkms:sae:` y `sae://`) —
   demo-star usa el legacy.
9. **Orden 3 → 4**: el binding de `ext_keys` de la Fase 4 consulta
   `PeerRegistry`; hasta la Fase 3 ese registry es escribible por el announce
   sin autenticar.
10. **k8s**: la distribución de certs debe ser explícita (secret compartido),
    no self-signed por pod — H-5 del audit fue exactamente eso.

---

## 6. Matriz de verificación

| Fase | Harness | Negativos nuevos |
|---|---|---|
| 1 | local-mesh keys smoke, testbed t10, demo-star | header forjado; SAE de otro DKMS |
| 2 | mesh.sh, t00, provision_certs T11 | cert cross-plane rechazado |
| 3 | mesh.sh, t30/t31 | rebind SAE con SAN ajeno |
| 4 | stress.sh, t20, t50 | ext_keys id≠SAN; ACK de peer equivocado |
| 5 | t40/t41, mesh mixto off/prefer | INIT forjado bajo `require` |
| 6 | orr unit tests, bootstrap_times.py | EstablishSecret rogue vs peer establecido |
| 7 | barrido completo t00–t50 | Drain desde no-localhost |
| 8 | unit (frame_mac, frame_auth, onion, onion_replay), mesh.sh N=4 qkd+pqc, CESGA n=10 | ciphertext manipulado; emisor suplantado; frame redirigido; headers reescritos; replay de frame y de NOTIFY; capa movida de época o de max_hops; reinicio del peer |
