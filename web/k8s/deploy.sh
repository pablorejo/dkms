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

KUBECTL_CMD="${KUBECTL:-kubectl}"
WEB_NAMESPACE="${WEB_NAMESPACE:-web-dkms}"
WEB_REPLICAS="${WEB_REPLICAS:-2}"
WEB_PULL_SECRET="${WEB_PULL_SECRET:-dockerhub-pull}"
WEB_INGRESS_CLASS="${WEB_INGRESS_CLASS:-nginx}"
WEB_INGRESS_PATH="${WEB_INGRESS_PATH:-/web}"
WEB_ORCH_COOKIE_SECURE="${WEB_ORCH_COOKIE_SECURE:-true}"
WEB_ORCH_REQUEST_TIMEOUT_MS="${WEB_ORCH_REQUEST_TIMEOUT_MS:-300000}"
WEB_RUNTIME_BASE_URL="${WEB_RUNTIME_BASE_URL:-}"
WEB_WAIT_TIMEOUT="${WEB_WAIT_TIMEOUT:-240s}"
INGRESS_CONTROLLER_NAMESPACE="${INGRESS_CONTROLLER_NAMESPACE:-ingress-nginx}"
INGRESS_CONTROLLER_SERVICE="${INGRESS_CONTROLLER_SERVICE:-ingress-nginx-controller}"
DOCKER_HUB_SERVER="${DOCKER_HUB_SERVER:-https://index.docker.io/v1/}"

if ! command -v "$KUBECTL_CMD" >/dev/null 2>&1; then
  echo "Error: no se encontro $KUBECTL_CMD" >&2
  exit 1
fi

if [[ -z "${DOCKER_HUB_USERNAME:-}" ]]; then
  echo "Error: DOCKER_HUB_USERNAME no esta definido" >&2
  exit 1
fi

if [[ -z "${DOCKER_HUB_TOKEN:-}" ]]; then
  echo "Error: DOCKER_HUB_TOKEN no esta definido" >&2
  exit 1
fi

WEB_IMAGE_REPO="${WEB_IMAGE_REPO:-docker.io/${DOCKER_HUB_USERNAME}/dkms-web}"
WEB_IMAGE_TAG_STABLE="${WEB_IMAGE_TAG_STABLE:-v1}"
WEB_IMAGE="${WEB_IMAGE:-${WEB_IMAGE_REPO}:${WEB_IMAGE_TAG_STABLE}}"

if [[ -z "${WEB_INGRESS_HOST:-}" ]]; then
  ingress_hostname="$("$KUBECTL_CMD" -n "$INGRESS_CONTROLLER_NAMESPACE" get svc "$INGRESS_CONTROLLER_SERVICE" -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)"
  ingress_ip="$("$KUBECTL_CMD" -n "$INGRESS_CONTROLLER_NAMESPACE" get svc "$INGRESS_CONTROLLER_SERVICE" -o jsonpath='{.status.loadBalancer.ingress[0].ip}' 2>/dev/null || true)"
  WEB_INGRESS_HOST="$ingress_hostname"
  if [[ -z "$WEB_INGRESS_HOST" ]]; then
    WEB_INGRESS_HOST="$ingress_ip"
  fi
  if [[ -z "$WEB_INGRESS_HOST" ]]; then
    echo "Error: no se pudo autodetectar WEB_INGRESS_HOST desde $INGRESS_CONTROLLER_NAMESPACE/$INGRESS_CONTROLLER_SERVICE" >&2
    echo "Define WEB_INGRESS_HOST manualmente o crea primero el servicio de ingress-nginx" >&2
    exit 1
  fi
fi

if [[ "$WEB_INGRESS_PATH" != /* ]]; then
  WEB_INGRESS_PATH="/$WEB_INGRESS_PATH"
fi

escape_sed_replacement() {
  printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}

render_manifest() {
  local manifest_file="$1"
  local ns image replicas pull_secret ingress_class ingress_host ingress_path cookie_secure orch_timeout runtime_base_url
  ns="$(escape_sed_replacement "$WEB_NAMESPACE")"
  image="$(escape_sed_replacement "$WEB_IMAGE")"
  replicas="$(escape_sed_replacement "$WEB_REPLICAS")"
  pull_secret="$(escape_sed_replacement "$WEB_PULL_SECRET")"
  ingress_class="$(escape_sed_replacement "$WEB_INGRESS_CLASS")"
  ingress_host="$(escape_sed_replacement "$WEB_INGRESS_HOST")"
  ingress_path="$(escape_sed_replacement "$WEB_INGRESS_PATH")"
  cookie_secure="$(escape_sed_replacement "$WEB_ORCH_COOKIE_SECURE")"
  orch_timeout="$(escape_sed_replacement "$WEB_ORCH_REQUEST_TIMEOUT_MS")"
  runtime_base_url="$(escape_sed_replacement "$WEB_RUNTIME_BASE_URL")"

  sed \
    -e "s|__WEB_NAMESPACE__|$ns|g" \
    -e "s|__WEB_IMAGE__|$image|g" \
    -e "s|__WEB_REPLICAS__|$replicas|g" \
    -e "s|__WEB_PULL_SECRET__|$pull_secret|g" \
    -e "s|__WEB_INGRESS_CLASS__|$ingress_class|g" \
    -e "s|__WEB_INGRESS_HOST__|$ingress_host|g" \
    -e "s|__WEB_INGRESS_PATH__|$ingress_path|g" \
    -e "s|__WEB_ORCH_COOKIE_SECURE__|$cookie_secure|g" \
    -e "s|__WEB_ORCH_REQUEST_TIMEOUT_MS__|$orch_timeout|g" \
    -e "s|__WEB_RUNTIME_BASE_URL__|$runtime_base_url|g" \
    "$manifest_file"
}

echo "[web-k8s] Namespace: $WEB_NAMESPACE"
echo "[web-k8s] Image: $WEB_IMAGE"
echo "[web-k8s] Ingress host: $WEB_INGRESS_HOST"
echo "[web-k8s] ORCH_COOKIE_SECURE: $WEB_ORCH_COOKIE_SECURE"
echo "[web-k8s] ORCH_REQUEST_TIMEOUT_MS: $WEB_ORCH_REQUEST_TIMEOUT_MS"
echo "[web-k8s] WEB_RUNTIME_BASE_URL: $WEB_RUNTIME_BASE_URL"

render_manifest "$SCRIPT_DIR/namespace.yaml" | "$KUBECTL_CMD" apply -f -

docker_secret_args=(
  create secret docker-registry "$WEB_PULL_SECRET"
  --docker-server="$DOCKER_HUB_SERVER"
  --docker-username="$DOCKER_HUB_USERNAME"
  --docker-password="$DOCKER_HUB_TOKEN"
  --dry-run=client
  -o yaml
)
if [[ -n "${DOCKER_HUB_EMAIL:-}" ]]; then
  docker_secret_args+=(--docker-email="$DOCKER_HUB_EMAIL")
fi
"$KUBECTL_CMD" -n "$WEB_NAMESPACE" "${docker_secret_args[@]}" | "$KUBECTL_CMD" apply -f -

render_manifest "$SCRIPT_DIR/deployment.yaml" | "$KUBECTL_CMD" apply -f -
render_manifest "$SCRIPT_DIR/service.yaml" | "$KUBECTL_CMD" apply -f -
render_manifest "$SCRIPT_DIR/ingress.yaml" | "$KUBECTL_CMD" apply -f -

echo "[web-k8s] Forzando rollout restart para refrescar imagen mutable: $WEB_IMAGE"
"$KUBECTL_CMD" -n "$WEB_NAMESPACE" rollout restart deployment/dkms-web
"$KUBECTL_CMD" -n "$WEB_NAMESPACE" rollout status deployment/dkms-web --timeout="$WEB_WAIT_TIMEOUT"

echo
echo "Web desplegada correctamente"
echo "Login:       https://$WEB_INGRESS_HOST/web/login"
echo "Simulations: https://$WEB_INGRESS_HOST/web/simulations"
