#!/usr/bin/env bash
# Build the five DKMS runtime container images from the workspace
# Dockerfile in a single pass.
#
# This script never pushes; tagging only. After the run, inspect the
# images with `docker images pablopio/{dkms,orr,qkc,sdn,quditto}`.
#
# Override:
#   TAG=mytag IMAGE_PREFIX=myrepo ./scripts/build-images.sh
#   ./scripts/build-images.sh qkc orr      # build only a subset
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

: "${TAG:=local}"
: "${IMAGE_PREFIX:=pablopio}"
# docker/Dockerfile es el contrato de imagen del despliegue multi-host: trae
# entrypoint.sh + render_config.py, que convierten el node.yml montado en la
# config nativa del binario. Dockerfile.workspace produce el binario pelado,
# que ese despliegue no puede arrancar (el qkc muere con "required arguments
# were not provided: --config"). Los cinco binarios se compilan igualmente de
# una pasada, en la etapa `build` compartida.
: "${DOCKERFILE:=$REPO_ROOT/docker/Dockerfile}"
: "${PLATFORM:=linux/amd64}"
: "${BUILDKIT_PROGRESS:=auto}"
export BUILDKIT_PROGRESS DOCKER_BUILDKIT=1

if [[ ! -f "$DOCKERFILE" ]]; then
    echo "Error: $DOCKERFILE not found" >&2
    exit 1
fi

ALL_BINARIES=(dkms orr qkc sdn quditto)
if [[ "$#" -eq 0 ]]; then
    BINARIES=("${ALL_BINARIES[@]}")
else
    BINARIES=("$@")
    for b in "${BINARIES[@]}"; do
        if ! printf '%s\n' "${ALL_BINARIES[@]}" | grep -qx "$b"; then
            echo "Error: unknown binary '$b'. Valid: ${ALL_BINARIES[*]}" >&2
            exit 2
        fi
    done
fi

cd "$REPO_ROOT"

echo "[build-images] tag=$TAG prefix=$IMAGE_PREFIX platform=$PLATFORM"
echo "[build-images] binaries: ${BINARIES[*]}"
echo

for binary in "${BINARIES[@]}"; do
    image="${IMAGE_PREFIX}/${binary}:${TAG}"
    echo "──── Building ${image} ────"
    docker build \
        --platform "$PLATFORM" \
        --file "$DOCKERFILE" \
        --build-arg "MODULE=${binary}" \
        --tag "$image" \
        .
    echo
done

echo "[build-images] Done. Resulting images:"
printf '%s\n' "${BINARIES[@]}" | while read -r b; do
    docker image inspect "${IMAGE_PREFIX}/${b}:${TAG}" \
        --format '{{.RepoTags}} size={{.Size}}' \
        2>/dev/null || echo "  (missing: ${IMAGE_PREFIX}/${b}:${TAG})"
done
