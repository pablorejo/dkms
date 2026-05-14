#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WEB_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$WEB_DIR/.." && pwd)"

if [[ -f "$REPO_ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1090
  source "$REPO_ROOT/.env"
  set +a
fi

DOCKER_CMD="${DOCKER:-docker}"
WEB_IMAGE_TAG_STABLE="${WEB_IMAGE_TAG_STABLE:-v1}"
WEB_IMAGE_TAG_LATEST="${WEB_IMAGE_TAG_LATEST:-latest}"
WEB_IMAGE_REPO="${WEB_IMAGE_REPO:-docker.io/${DOCKER_HUB_USERNAME:-}/dkms-web}"
WEB_DOCKER_CONTEXT="${WEB_DOCKER_CONTEXT:-$WEB_DIR}"
WEB_DOCKERFILE="${WEB_DOCKERFILE:-$SCRIPT_DIR/Dockerfile}"
ACTION="${1:-build-and-push}"

if ! command -v "$DOCKER_CMD" >/dev/null 2>&1; then
  echo "Error: no se encontro $DOCKER_CMD" >&2
  exit 1
fi

if [[ -z "${DOCKER_HUB_USERNAME:-}" ]]; then
  echo "Error: DOCKER_HUB_USERNAME no esta definido (usa .env del repo raiz)" >&2
  exit 1
fi

if [[ -z "${DOCKER_HUB_TOKEN:-}" ]]; then
  echo "Error: DOCKER_HUB_TOKEN no esta definido (usa .env del repo raiz)" >&2
  exit 1
fi

if [[ ! -f "$WEB_DOCKERFILE" ]]; then
  echo "Error: no existe Dockerfile en $WEB_DOCKERFILE" >&2
  exit 1
fi

stable_image="${WEB_IMAGE_REPO}:${WEB_IMAGE_TAG_STABLE}"
latest_image="${WEB_IMAGE_REPO}:${WEB_IMAGE_TAG_LATEST}"

normalize_repo_for_scope() {
  local repo="$1"
  repo="${repo#docker.io/}"
  repo="${repo#index.docker.io/}"
  repo="${repo#registry-1.docker.io/}"
  printf '%s' "$repo"
}

check_push_scope() {
  local scoped_repo response has_token
  scoped_repo="$(normalize_repo_for_scope "$WEB_IMAGE_REPO")"
  response="$(
    curl -sS \
      -u "$DOCKER_HUB_USERNAME:$DOCKER_HUB_TOKEN" \
      "https://auth.docker.io/token?service=registry.docker.io&scope=repository:${scoped_repo}:pull,push"
  )"
  has_token="$(
    RESPONSE_JSON="$response" python3 - <<'PY'
import json
import os
try:
    data = json.loads(os.environ["RESPONSE_JSON"])
except Exception:
    print("0")
    raise SystemExit(0)
print("1" if isinstance(data, dict) and "token" in data else "0")
PY
  )"
  if [[ "$has_token" != "1" ]]; then
    echo "Error: el token de Docker Hub no tiene permisos de push para ${scoped_repo}." >&2
    echo "Crea/actualiza un token con permisos Read & Write para ese repositorio y vuelve a ejecutar." >&2
    exit 1
  fi
}

login_dockerhub() {
  echo "$DOCKER_HUB_TOKEN" | "$DOCKER_CMD" login --username "$DOCKER_HUB_USERNAME" --password-stdin
}

build_images() {
  "$DOCKER_CMD" build \
    -f "$WEB_DOCKERFILE" \
    -t "$stable_image" \
    -t "$latest_image" \
    "$WEB_DOCKER_CONTEXT"
}

push_images() {
  "$DOCKER_CMD" push "$stable_image"
  "$DOCKER_CMD" push "$latest_image"
}

case "$ACTION" in
  build)
    login_dockerhub
    build_images
    ;;
  push)
    check_push_scope
    login_dockerhub
    push_images
    ;;
  build-and-push)
    check_push_scope
    login_dockerhub
    build_images
    push_images
    ;;
  *)
    echo "Error: accion invalida '$ACTION'. Usa: build | push | build-and-push" >&2
    exit 1
    ;;
esac

echo "Web image OK: $stable_image, $latest_image"
