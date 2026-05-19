# iter_008 — OBJ-010 (Fase D: docker build local)

**Fecha:** 2026-05-19
**Objetivo:** OBJ-010.

## Qué se hizo

Build local de las 3 imágenes nuevas:

| Imagen | ID | Tamaño |
|---|---|---:|
| `pablopio/sdn:v8` | `0c95e4a6dfc7` | 138 MB |
| `pablopio/orr:v3` | `f1a3979702f8` | 137 MB |
| `pablopio/qkc:v3` | `6eb465c440f2` | 136 MB |

Comandos ejecutados (todos exitosos):
```bash
docker build -t pablopio/sdn:v8 -f sdn/Dockerfile .
docker build -t pablopio/orr:v3 -f orr/Dockerfile .
docker build -t pablopio/qkc:v3 -f qkc/Dockerfile .
```

Dockerfiles existentes y sin modificación necesaria (cambios fueron solo código Rust). Logs en `logs/docker_build_{sdn,orr,qkc}.log`.

## Rollback path intacto (R-010)

Verificado `docker images pablopio/{sdn,orr,qkc}`. Tags previos siguen en local:

- `sdn`: v7, v7-8ae21d7, v6, v5, v5-147b279, v2-eb110e7, v2-2d21944, v2, v2-dd93806, etc.
- `orr`: v7, v7-8ae21d7, v5, v5-147b279, v2, v2-dd93806, v2-eb110e7, v2-2d21944, v2-36dfb0a, local.
- `qkc`: v7, v7-8ae21d7, v5, v5-147b279, v4, v2, v2-dd93806, v2-2d21944, v2-eb110e7, local.

El rollback `set env deploy/orchestator SDN_IMAGE=pablopio/sdn:v7 ORR_IMAGE=pablopio/orr:v2 QKC_IMAGE=pablopio/qkc:v4` está disponible (las imágenes existen en local, deberían estar en el registry remoto también — verificable con `docker manifest inspect` antes del push).

## Riesgo identificado — tag colisión registry

Los tags `:v8` / `:v3` **ya existían en este host local** (de iters/builds previos, fechas distintas). Esto NO es problema en local (Docker reasigna el tag al nuevo image ID), PERO R-014 prohíbe sobrescribir tags en el **registry remoto**.

**Verificación pendiente para OBJ-012/013:** `docker manifest inspect pablopio/sdn:v8` contra `docker.io/pablopio/sdn`. Si retorna éxito (ya pusheado) → incrementar a `:v8.1` antes de pushear. Si retorna `not found` → seguir adelante con `:v8`.

Esa verificación es parte del pre-flight check (OBJ-012). En esta iter solo registramos el riesgo.

## Próximo

OBJ-011: smoke test local. Opciones evaluadas:
- (a) `docker-compose` si existe — preferido.
- (b) `cargo run -p <crate>` directo con env vars.

Setup: topología 3 nodos (ring), 2 DKMS, MULTIPATH_ENABLED=true en el ORR, enviar 1 mensaje cruzando ≥2 hops y verificar logs:
- ORR origen: `multipath path selected qkc_hops=[..]`.
- QKC-1: `multipath next_hop=X consumed from header`.
- ORR destino: recibe OK.

## Bloqueos

Ninguno. Pre-flight R-014 sigue avanzando: cargo verde ✓, clippy verde ✓, docker build ✓. Faltan smoke (OBJ-011) y verificación tag-no-existe (OBJ-012) antes del push (OBJ-013).
