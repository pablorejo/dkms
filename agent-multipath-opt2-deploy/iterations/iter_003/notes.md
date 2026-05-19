# iter_003 — OBJ-003 + decisión OBJ-004 (cableado en ORR send)

**Fecha:** 2026-05-19
**Objetivos:** OBJ-003 (wiring `pick_multipath_qkc_hops`) + OBJ-004 (decisión arquitectónica).

## Decisión OBJ-004 — opción (a) extendida via `app_header`

Tres opciones evaluadas para que el ORR conozca `src_dkms` y `dst_dkms` al invocar `pick_multipath_qkc_hops`:

| Opción | Cambio API | Cambio DKMS | Cambio proto | Backwards-compat |
|---|---|---|---|---|
| (a) ampliar firma de `send_message` con `src_dkms`/`dst_dkms` | Sí | Sí (R-003 lo permite explícito) | Sí (`SendMessageRequest`) | NO |
| (b) lookup local `orr_id → dkms_id` en `OrrService` | No | No | No | Sí pero requiere mapping inverso que no existe |
| **(a) extendida — piggyback en `app_header`** | **No** | **Sí (mínimo, R-003 OK)** | **No** | **Sí (si keys ausentes, fallback)** |

Elegida: **(a) extendida**. El DKMS pone `app_header["src_dkms"]` y `app_header["dst_dkms"]` antes de invocar a `OrrControl::SendMessage`. Si las keys están ausentes (e.g. caller legacy), el ORR no activa multipath y queda en fallback single-path. Cero cambios de proto ni signature.

**El cambio en DKMS NO se hace en esta iter** — es un OBJ separado (Fase B podría extenderse o documentarse como tarea de OBJ-004 satisfecho con esta decisión). Por ahora el ORR queda listo y backwards-compatible: si nadie pasa esas keys, el comportamiento es idéntico al actual.

## Qué se hizo

### `orr/src/service.rs` — nuevo helper

`async fn compute_qkc_path_header(&self, app_header: &BTreeMap<String, String>) -> Vec<u8>`:

1. Lee env var `MULTIPATH_ENABLED` (R-015 opt-in). Si distinto de `"true"`, retorna `Vec::new()` (sin multipath).
2. Lee `app_header.get("src_dkms")` y `app_header.get("dst_dkms")`. Si faltan, retorna `Vec::new()`.
3. Llama `self.pick_multipath_qkc_hops(src_dkms, dst_dkms).await`. Si Err (warn log) o `Ok(None)`, retorna `Vec::new()`.
4. Parsea `Vec<String> → Vec<u32>` con `parse::<u32>()`. Si algún parse falla (warn log), retorna `Vec::new()`.
5. Caso exitoso: `wire::encode_qkc_path(&parsed)` + `debug!` log.

### Cableado en `send_passthrough` y `send_onion_frame`

Antes de construir el `Frame`, llaman al helper y asignan el resultado a `frame.header_qkc_mp`:

- `send_passthrough` (max_hops=0): siempre se cablea — passthrough crudo también puede beneficiarse de multipath.
- `send_onion_frame` (max_hops=1 PQC E2E + max_hops≥2 / -1 multi-hop onion): siempre se cablea.

**Cero cambios** en la lógica onion, encrypt, frame layout, wire format binario.

### Sin cambios

- Proto `proto/orr.proto`: intacto.
- `grpc_server.rs`: intacto.
- `dkms/src/*`: intacto (R-003 respetado).
- `qkc/src/*`: pendiente Fase C (OBJ-006/007/008).

## Verificación

- `cargo build -p orr --release` → OK.
- `cargo test -p orr --release --lib` → **69 passed** (sin regresiones, los tests previos pasan tal cual).
- `cargo clippy -p orr --release --all-targets -- -D warnings` → verde.

## Cómo se verifica end-to-end (cuando QKC esté wireado en Fase C)

```
MULTIPATH_ENABLED=true cargo run -p orr ...
DKMS llama OrrControl::SendMessage con app_header={"src_dkms":"d1","dst_dkms":"d2"}
ORR computa path → frame.header_qkc_mp = msgpack(qkc_path)
QKC consume hop a hop (pendiente Fase C)
```

## Próximo

OBJ-005: tests de integración del wiring en ORR. Casos:
1. cache hit + multipath ON + app_header con src/dst → frame con header_qkc_mp no vacío con bytes correctos.
2. cache hit + multipath OFF (env unset) → header_qkc_mp vacío.
3. cache hit + multipath ON + app_header sin src_dkms → header_qkc_mp vacío.
4. cache miss + multipath ON + sin SDN client → header_qkc_mp vacío (no panic).
5. Verificar bytes decodificados con `wire::decode_qkc_path` = qkc_path esperado.

## Bloqueos

Ninguno.
