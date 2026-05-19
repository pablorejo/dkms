# K-Splittable MCF — Opción 2: source routing en `Frame.header_qkc_mp`

> Sucesor de `memory/project_multipath_design.md`. Aquel documento describe el
> **solver** (Fases A/B/C del agente previo, ya en `main`). Este documento describe
> el **wiring real** (cómo el path muestreado llega del ORR origen al QKC y se
> consume hop a hop) que completa la migración end-to-end.

## 1. Problema que resuelve este documento

Tras Fases A/B/C del agente previo (`agent-multipath-sdn/`):

- El SDN calcula K paths con ratios `omega` (solver max-min weighted multi-path).
- El RPC `GetPathsWithRatios(src_dkms, dst_dkms)` los publica.
- El ORR cliente tiene `pick_multipath_qkc_hops(src_dkms, dst_dkms)` con
  `AliasSampler` precomputado y cache invalidation on `TopologyEvent`.

Pero **el camino no llega al QKC**. `OrrService::send_message` sigue
preguntando un único `orr_path` (single-path) y el QKC enruta por su
forwarding table destination-based.

**La pregunta es: ¿cómo le decimos al QKC qué camino seguir?**

## 2. Por qué opción 2 (source routing en cabecera) y no las otras

### Opción descartada A — Per-flow table en QKC

QKC con tabla `flow_id → next_hop`. Pros: cabecera mínima. **Contras**:

- Estado O(N²) por QKC (con N=50 DKMSs son 2 500 entradas; con K=3 multipath
  son 7 500).
- Cada `topology_changed` invalida tablas en todos los QKCs.
- Race conditions: paquetes en vuelo con label viejo aterrizando en QKC con
  tabla nueva.
- Coordinación SDN ↔ QKCs compleja.

### Opción descartada B — Label MPLS

Cabecera lleva label 16-32 bits. Tabla `label → (next_hop, swap)` por QKC.

Esto es **MPLS clásico**. Funciona en operadores telco grandes pero exige:

- Label allocator (centralizado o distribuido — RFC 3036 LDP, 132 páginas).
- Race conditions tipo "label reuse" cuando recompute saca un label viejo.
- Race conditions tipo "label en vuelo" en topology events.
- Debugging distribuido (5+ tablas en 5+ nodos para entender "por qué este
  paquete fue allí").

Trade-off MPLS vs Opción 2 a escala 50 DKMSs (no 10 000):

| | Opción 2 | MPLS |
|---|---|---|
| Líneas a implementar | ~180 | ~600-1000 |
| Estado por QKC | 0 nuevo | FIB completa |
| Coordinación SDN↔QKC | Cero (ORR escribe el path) | Compleja (FIB push + invalidation) |
| Cabecera por paquete | ~80 bytes | ~4 bytes |
| Race conditions | 0 | 3-4 escenarios |

MPLS es overkill. Descartado.

### Opción descartada C — `max_hops=-1` (cebolla multi-hop)

Forzar onion capa-a-capa por todo el path SDN. Cada ORR intermedio pela su
capa, lee "siguiente hop = X" y le da al QKC `dest_final = qkc(X)`.

Pros: cero cambios en QKC. **Contras**:

- Solo funciona si **cada QKC del path tiene un ORR co-localizado**. En
  topologías estrella con ORR solo en hojas, no aplica.
- Latencia: ML-KEM decap + re-encap por hop, peor para QKD time-sensitive.
- Más bytes (1 capa onion por hop, no por extremo).

Para el deploy actual con `max_hops=1` (PQC E2E) no aplica naturalmente.
Descartado.

### Opción adoptada — Source routing en `Frame.header_qkc_mp`

El ORR origen muestrea el path completo via `pick_multipath_qkc_hops` y lo
escribe en el campo `Frame.header_qkc_mp` (ya existente, msgpack, vacío en la
versión desplegada hoy, reservado para "metadatos del QKC"). Cada QKC en
la ruta pop-ea el primer elemento del path como next-hop y reenvía. El QKC
queda **stateless por flow**.

**Ventajas:**

- QKC sigue stateless por flow (filosofía del proyecto).
- Wire binary format NO cambia. Solo el contenido del campo opcional.
- Per-paquete sampling trivial: cada paquete puede elegir un path distinto.
- Backwards-compatible: QKC sin el código nuevo recibe un `header_qkc_mp` y
  lo ignora; un ORR sin el código nuevo no lo escribe.
- Funciona con `max_hops=1` (modo PQC E2E actual) sin requerir ORRs
  intermedios.
- ~180 líneas de código total para wiring.

**Desventajas asumidas:**

- Cabecera ~80 bytes extra (~16 bytes por hop × ~5 hops promedio). Para keys
  de 32-64 bytes, overhead ~2.5× — asumible para keys sensibles al tiempo.

## 3. Formato exacto msgpack

El campo `Frame.header_qkc_mp: Vec<u8>` lleva un msgpack map con una sola
clave:

```
{"qkc_path": [u32, u32, u32, ...]}
```

Tipo: `BTreeMap<String, Vec<u32>>` (msgpack-rs encode).

**Bytes esperados** (path `[2, 3, 4]`):

```
0x81           # map fixmap con 1 par clave-valor
0xa8 q k c _ p a t h   # str8 "qkc_path"
0x93           # array fixarray de 3 elementos
0x02 0x03 0x04 # u32 fixmints
```

Total ~14 bytes para path de 3 hops; ~30 bytes para path de 5 hops.

**Path vacío (`vec![]`)**:

Por convención **NO se escribe el `header_qkc_mp`** — se deja `Vec::new()` (0
bytes). El QKC distingue "sin source routing" (bytes vacíos) del "path con 0
elementos" (que no tiene sentido semánticamente).

**Conversión de tipos `String -> u32`:**

El RPC `GetPathsWithRatios` devuelve `qkc_hops: Vec<String>` (per
`proto/sdn.proto`). El ORR cliente debe parsear cada String a u32 antes de
encodear:

```rust
let path_u32: Vec<u32> = path_strings
    .iter()
    .map(|s| s.parse::<u32>())
    .collect::<Result<Vec<_>, _>>()?;
frame.header_qkc_mp = wire::encode_qkc_path(&path_u32);
```

Si algún `parse::<u32>()` falla, el ORR **debe** loguear `warn` y caer al
fallback single-path (NO escribir `header_qkc_mp`). No es razonable que un
qkc_id no sea numérico, pero la verificación protege contra topologías
malformadas.

## 4. Diagrama end-to-end

```
Topología ejemplo:
    A (qkc=1) ──── B (qkc=2) ──── F (qkc=4)
                  │
                  └── E (qkc=3) ──── F

Path muestreado: [2, 3, 4]  (vía qkc=2 → qkc=3 → qkc=4)

┌──────────────────────────────────────────────────────────────────────┐
│ 1. DKMS A: send_keys(dest=DKMS_F, payload)                           │
│    → ORR A: send_message(dest_orr=ORR_F, max_hops=1, payload)        │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ 2. ORR A:                                                            │
│    a) (src_dkms, dst_dkms) = (DKMS_A, DKMS_F)                        │
│    b) pick_multipath_qkc_hops(DKMS_A, DKMS_F) → Some(["2","3","4"])  │
│    c) Parse a u32 → [2, 3, 4]                                        │
│    d) frame.header_qkc_mp = encode_qkc_path(&[2, 3, 4])              │
│    e) frame.payload = cebolla onion contra ORR_F (1 capa, PQC E2E)   │
│    f) frame.dest_final = qkc_id_de(ORR_F) = 4                        │
│    g) Enviar a QKC local (=1)                                        │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼ TCP binario
┌──────────────────────────────────────────────────────────────────────┐
│ 3. QKC=1 (handle_local_send):                                        │
│    a) header_qkc_mp no vacío → wire::pop_qkc_path_next_hop(bytes)    │
│       → Ok((2, encode([3, 4])))                                      │
│    b) next_hop = 2                                                   │
│    c) frame.header_qkc_mp = encode([3, 4])                           │
│    d) Enviar a QKC=2                                                 │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ 4. QKC=2 (handle_incoming):                                          │
│    a) Mismo patrón: pop → (3, encode([4]))                           │
│    b) Enviar a QKC=3                                                 │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ 5. QKC=3 (handle_incoming):                                          │
│    a) Pop → (4, encode([]))                                          │
│    b) frame.header_qkc_mp = bytes vacíos (path agotado)              │
│    c) Enviar a QKC=4                                                 │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ 6. QKC=4 (handle_incoming):                                          │
│    a) header_qkc_mp vacío → fallback routing table                   │
│    b) dest_final == qkc_id_local (4) → entregar al ORR local (ORR_F) │
└──────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌──────────────────────────────────────────────────────────────────────┐
│ 7. ORR F: pela capa onion → entrega payload al DKMS F                │
└──────────────────────────────────────────────────────────────────────┘
```

## 5. Hand-off entre componentes

| Quién | Qué hace | Cómo |
|---|---|---|
| **SDN solver** | Calcula `omega_p` por path por commodity. | `weighted_maxmin` ya itera sobre `(commodity, path_idx)`. Sin cambios. |
| **SDN RPC** | Publica K paths con ratios al ORR. | `GetPathsWithRatios` ya devuelve `qkc_hops: Vec<String>` por path. Sin cambios. |
| **ORR cliente cache** | Cachea K paths + sampler precomputado. | `paths_cache_multipath` + invalidation on `TopologyEvent`. Sin cambios. |
| **ORR origen `send_message`** | **NUEVO** — muestrea path y escribe el msgpack. | `pick_multipath_qkc_hops` + parse `String→u32` + `wire::encode_qkc_path` + asignar a `frame.header_qkc_mp`. |
| **wire helper `encode_qkc_path`** | **NUEVO** — encode `Vec<u32>` a msgpack bytes. | `rmp_serde::to_vec` sobre `BTreeMap<&str, Vec<u32>>` con key `"qkc_path"`. |
| **wire helper `pop_qkc_path_next_hop`** | **NUEVO** — pop primer u32 + re-encode resto. | Decode → tomar `path[0]` → re-encode `path[1..]`. Si vacío, retornar bytes vacíos. |
| **QKC `handle_local_send`** | **NUEVO** — if `header_qkc_mp` no vacío, pop next_hop; else fallback routing. | Llamada a `wire::pop_qkc_path_next_hop`. Si Ok → usar next_hop. Si Err o vacío → routing table. |
| **QKC `handle_incoming`** | **NUEVO** — mismo patrón que `handle_local_send` para frames intermedios. | Idem. |
| **DKMS** | Sin cambios. | Sigue invocando `ORR::send_message(dest_orr, payload, max_hops, app_header)` como siempre. |
| **Wire binary format** | Sin cambios. | `Frame.header_qkc_mp: Vec<u8>` ya existe; solo cambia el contenido del bytes. |

## 6. Feature flag — opt-in (R-015)

El comportamiento multipath es **opcional** y por defecto **OFF**, controlado
por una env var `MULTIPATH_ENABLED` que lee el ORR al boot (o config flag).

- `MULTIPATH_ENABLED=true`: ORR invoca `pick_multipath_qkc_hops` y escribe
  `header_qkc_mp` cuando hay path.
- `MULTIPATH_ENABLED=false` o no set: ORR ni siquiera intenta el sampling.
  Comportamiento idéntico a `pablopio/orr:v2` (single-path legacy).

Esto garantiza que un rollback es trivial:

```
kubectl set env deploy/orchestator MULTIPATH_ENABLED-
```

(El `-` borra la env var.) En el siguiente rollout, los pods nuevos vuelven
a single-path.

Las imágenes `pablopio/orr:v3`, `pablopio/sdn:v8`, `pablopio/qkc:v3`
**incluyen el código nuevo, pero NO lo activan por defecto**. La activación
es responsabilidad del orchestator que setea la env var.

## 7. Backwards-compatibility

- **QKC viejo (pablopio/qkc:vX) recibe `header_qkc_mp` con bytes no vacíos**:
  msgpack es un blob opaco para el QKC viejo. Lo ignora y enruta por su
  routing table normal. **Funciona, pero NO se beneficia del multipath**
  (el path muestreado por el ORR se desperdicia).
- **QKC nuevo (`:v3`) recibe `header_qkc_mp` vacío**: fallback a routing
  table actual. Idéntico a comportamiento legacy.
- **ORR viejo (pablopio/orr:v2) con QKC nuevo (`:v3`)**: el ORR no escribe
  `header_qkc_mp`; el QKC nuevo simplemente no lo lee. Single-path total.
- **ORR nuevo (`:v3`) con QKC viejo**: el ORR escribe el path; el QKC viejo
  lo ignora y rutea por destino. Funciona pero subóptimo.

**El multipath se "activa" cuando los tres binarios nuevos están desplegados
JUNTOS y `MULTIPATH_ENABLED=true`.** Cualquier combinación parcial degrada
gracefully a single-path.

## 8. Lo que NO se toca

- Wire binary format (`Frame` struct, encoding, framing). R-002.
- `dkms/src/*`. R-003.
- API ETSI 014/020 DKMS↔SAE.
- Onion encryption del ORR (sigue siendo `max_hops=1` PQC E2E por defecto).
- Forwarding table del QKC para `dest_final` (solo se consulta cuando el
  path está vacío — fallback).
- Topology mutation (R-010 del proyecto: topología inmutable hasta
  reinicio del SDN).

## 9. Riesgos identificados

- **Parsing `String → u32` falla**: mitigado con fallback a single-path +
  warn log. No bloqueante.
- **Path muestreado tiene un qkc_id no alcanzable**: el QKC con next_hop
  inválido cae a su routing table normal — same as not finding the next
  hop. Tracing::warn registra la anomalía para debugging.
- **`header_qkc_mp` corrupto (bytes no msgpack válidos)**: `decode` falla
  → QKC fallback a routing table. Sin crash. El paquete llega al destino
  por shortest path. Tracing::warn registra la anomalía.
- **Race entre cache invalidation y sampling**: `paths_cache_multipath` es
  `Arc<RwLock>`; un read durante un write se serializa con la implementación
  de parking_lot. Sin race.
- **Path muestreado con bucle (e.g. `[2, 3, 2, 4]`)**: el solver de SDN
  filter_overlapping_paths NO debería producir bucles (paths son simples
  por construcción Yen's). Si por algún motivo ocurre, el QKC popea y
  reenvía hasta agotar el path; el bucle físico se traduce en TTL muerto
  por el `quic_route` del QKC (no implementado, asumimos paths simples).

## 10. Criterios de éxito de este wiring

Ver `.objetives.md` Fase F y R-017 para los criterios cuantitativos. En
resumen:

- **M1 spread** cae ≥ 30 % vs baseline en ≥ 2 de 3 topologías (mesh, random,
  bridge).
- **M5 starvation continua** cae ≥ 50 % vs baseline en ≥ 2 de 3.
- **M3 throughput total** NO cae > 10 % en ninguna topología.
- Tests cargo verde.
- Imágenes Docker buildable.

Si <2/3 topologías cumplen → `Estado: BLOQUEADO_CRITERIOS_NO_CUMPLIDOS`,
NO auto-tune (R-016).

## 11. Referencias

- Documento de diseño del solver: `memory/project_multipath_design.md`.
- Discusión arquitectónica con el usuario: chat 2026-05-18, decisión adoptada
  tras descartar opción A (per-flow table) y opción B (MPLS).
- Commits del agente previo en `main`: `f8d54af`, `b5ce2e3`, `509fae5`.
- Agente complementario que ejecuta este wiring: `agent-multipath-opt2-deploy/`.
