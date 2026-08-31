# Deployment

El camino mantenido es **multi-host por institución con Docker**: una imagen
por módulo, cada institución rellena un `node.yml` corto y hace
`docker compose up`. Todo está en [`docker/README.md`](../docker/README.md) —
imágenes, compose por módulo (`docker/compose/`), formato del `node.yml`,
puertos y firewall.

## Local dev (sin contenedores)

```bash
./scripts/build-all.sh          # cargo build --release del workspace

./scripts/run-sdn.sh            # cada módulo en su terminal
./scripts/run-qkc.sh
./scripts/run-orr.sh
./scripts/run-dkms.sh
./scripts/run-quditto.sh
```

Cada módulo carga `<module>/config/default.toml` + `local.toml` (gitignored)
vía `CONFIG_DIR`, con overrides por env (`DKMS__...`, separador `__`). La
excepción es el QKC, que toma `--config <path>` (el run script pasa
`qkc/config/default.toml` por defecto) y quditto, que va por flags/env
`QUDITTO_*`. Demos multi-módulo en `scripts/demo-3qkc/` y
`scripts/demo-star/`; la malla local completa en `tests/local-mesh/mesh.sh`.

## Imágenes

```bash
make images        # build local de las 5 imágenes (docker/Dockerfile)
make push          # push con TAG/IMAGE_PREFIX
# o multi-arch:
IMAGE_PREFIX=tuusuario docker buildx bake -f docker/docker-bake.hcl --push
```

Solo `docker/Dockerfile` produce imágenes desplegables (lleva
`entrypoint.sh` + `render_config.py`). `docker/Dockerfile.workspace` compila
el binario pelado y NO sirve para compose.

## Certificados

```bash
docker/gen-certs.sh <node_id> <ip> ./certs    # ML-DSA-65 por defecto
```

Dos raíces (docs/SECURITY.md §2): `sae-ca` para SAEs, `net-ca` para
peers/control plane. Un juego pre-generado tiene que colgar de UNA sola CA
— `mesh.sh` lo comprueba (AKI vs SKI).

## Kubernetes

Los manifests `<module>/k8s/` se retiraron el 2026-08-31 (eran pre-node.yml
y no arrancaban). Si hace falta k8s: manifests que monten `node.yml`
(ConfigMap) y certs (Secret) bajo `/config`, imagen
`${IMAGE_PREFIX}/<rol>:TAG`, puertos actuales (QKC 20000-20002, ORR
20003-20004, DKMS 20005-20009, SDN 19000/19002/19010, quditto 20010) y
nunca un Service sobre el 20007 (DkmsControl es loopback).

## Observabilidad

Prometheus `/metrics` (sin auth, responde a cualquier path — red interna):

| Módulo  | Puerto |
|---------|--------|
| orr     | 20004  |
| dkms    | 20008  |
| sdn     | 19010  |

El QKC no expone Prometheus: su estado va por el HTTP admin
(`:20002/stats`). quditto expone `/healthz` en su puerto HTTP (20010).
El estado del DKMS se lee del log `generator.state` (cada 5 s).
