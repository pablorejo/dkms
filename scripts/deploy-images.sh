#!/usr/bin/env bash
# Build (and optionally push) the deployable images for a subset of
# components. Used by `make deploy*` targets.
#
# Components: dkms orr qkc sdn quditto orchestrator authz web
#
# Usage:
#   scripts/deploy-images.sh [--push] [--no-cache] <component> [<component>...]
#   scripts/deploy-images.sh --push web orchestrator
#
# Environment:
#   TAG=v1           - mutable tag applied to every image (default: local)
#   IMMUTABLE_TAG    - if set, every image is also tagged with this
#   IMAGE_PREFIX=pablopio
#   PLATFORM=linux/amd64
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

: "${TAG:=local}"
: "${IMMUTABLE_TAG:=}"
: "${IMAGE_PREFIX:=pablopio}"
: "${PLATFORM:=linux/amd64}"
PUSH=0
NO_CACHE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --push)     PUSH=1; shift ;;
        --no-cache) NO_CACHE=1; shift ;;
        --)         shift; break ;;
        -*)         echo "unknown flag: $1" >&2; exit 2 ;;
        *)          break ;;
    esac
done

if [[ $# -eq 0 ]]; then
    echo "Error: at least one component is required" >&2
    echo "Usage: $0 [--push] [--no-cache] <component> [...]" >&2
    exit 2
fi

# Map component → docker image name (without prefix), file, target, context.
declare -A IMAGE_NAME=(
    [dkms]=dkms       [orr]=orr           [qkc]=qkc       [sdn]=sdn   [quditto]=quditto
    [orchestrator]=orchestator   # historical typo preserved
    [authz]=authz
    [web]=dkms-web
)

declare -A IMAGE_FILE=(
    [dkms]=docker/Dockerfile.workspace
    [orr]=docker/Dockerfile.workspace
    [qkc]=docker/Dockerfile.workspace
    [sdn]=docker/Dockerfile.workspace
    [quditto]=docker/Dockerfile.workspace
    [orchestrator]=orchestrator/Dockerfile
    [authz]=orchestrator/authz/Dockerfile
    [web]=web/docker/Dockerfile
)

declare -A IMAGE_TARGET=(
    [dkms]=image-dkms
    [orr]=image-orr
    [qkc]=image-qkc
    [sdn]=image-sdn
    [quditto]=image-quditto
)

declare -A IMAGE_CONTEXT=(
    [dkms]=.
    [orr]=.
    [qkc]=.
    [sdn]=.
    [quditto]=.
    [orchestrator]=orchestrator
    [authz]=orchestrator
    [web]=web
)

BUILD_FLAGS=("--platform" "$PLATFORM")
[[ "$NO_CACHE" -eq 1 ]] && BUILD_FLAGS+=("--no-cache")

ensure_docker_login() {
    if [[ "$PUSH" -eq 0 ]]; then return; fi
    if docker info 2>/dev/null | grep -q "Username:"; then return; fi
    if [[ -n "${DOCKER_HUB_USERNAME:-}" && -n "${DOCKER_HUB_TOKEN:-}" ]]; then
        echo "[deploy-images] docker login..." >&2
        echo "$DOCKER_HUB_TOKEN" | docker login --username "$DOCKER_HUB_USERNAME" --password-stdin >/dev/null
    else
        echo "[deploy-images] Warning: --push set but no DOCKER_HUB_* in env; trusting existing docker auth" >&2
    fi
}
ensure_docker_login

build_one() {
    local comp="$1"
    local name="${IMAGE_NAME[$comp]:-}"
    local file="${IMAGE_FILE[$comp]:-}"
    local context="${IMAGE_CONTEXT[$comp]:-.}"
    local target="${IMAGE_TARGET[$comp]:-}"

    if [[ -z "$name" || -z "$file" ]]; then
        echo "Error: unknown component '$comp'" >&2
        exit 2
    fi

    local image="${IMAGE_PREFIX}/${name}:${TAG}"
    local args=("${BUILD_FLAGS[@]}" "--file" "$file" "--tag" "$image")
    if [[ -n "${IMMUTABLE_TAG}" ]]; then
        args+=("--tag" "${IMAGE_PREFIX}/${name}:${IMMUTABLE_TAG}")
    fi
    [[ -n "$target" ]] && args+=("--target" "$target")
    args+=("$context")

    echo "──── build ${comp} → ${image} ────"
    docker build "${args[@]}"

    if [[ "$PUSH" -eq 1 ]]; then
        docker push "$image"
        if [[ -n "${IMMUTABLE_TAG}" ]]; then
            docker push "${IMAGE_PREFIX}/${name}:${IMMUTABLE_TAG}"
        fi
    fi
}

for comp in "$@"; do
    build_one "$comp"
done

echo "[deploy-images] done."
