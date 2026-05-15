#!/usr/bin/env bash
# Applies / re-applies manifests for the control plane components.
#
# Usage:
#   scripts/k8s-apply.sh                            # apply everything
#   scripts/k8s-apply.sh authz orchestrator web     # subset
#   scripts/k8s-apply.sh --rollout authz            # apply + force rollout restart
#
# Variables:
#   TAG=v2                    image tag to inject into deployments
#   INGRESS_HOST=dkms2.pablopiorejoiglesias.es     replaces __INGRESS_HOST__
#   KUBECTL=kubectl
#   WAIT_TIMEOUT=240s
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [[ -f .env ]]; then
    set -a
    # shellcheck disable=SC1091
    source .env
    set +a
fi

: "${TAG:=v2}"
: "${KUBECTL:=kubectl}"
: "${WAIT_TIMEOUT:=240s}"
: "${INGRESS_HOST:=dkms2.pablopiorejoiglesias.es}"
ROLLOUT=0

COMPONENTS=()
while [[ $# -gt 0 ]]; do
    case "$1" in
        --rollout) ROLLOUT=1; shift ;;
        -*)        echo "unknown flag: $1" >&2; exit 2 ;;
        *)         COMPONENTS+=("$1"); shift ;;
    esac
done
if [[ "${#COMPONENTS[@]}" -eq 0 ]]; then
    COMPONENTS=(authz orchestrator web manifests)
fi

echo "[k8s-apply] TAG=$TAG INGRESS_HOST=$INGRESS_HOST"

# Render manifest: substitute image tag for a given repo AND __INGRESS_HOST__.
render_manifest() {
    local file="$1"
    local image_repo="${2:-}"   # empty for ingress-only manifests
    local sed_args=("-E")
    if [[ -n "$image_repo" ]]; then
        sed_args+=("-e" "s|(image:[[:space:]]*)${image_repo}:[^[:space:]]+|\\1${image_repo}:${TAG}|g")
    fi
    sed_args+=("-e" "s|__INGRESS_HOST__|${INGRESS_HOST}|g")
    sed "${sed_args[@]}" "$file"
}

apply_authz() {
    echo "── apply authz ──"
    render_manifest "$REPO_ROOT/orchestrator/ingress/authz-deployment.yaml" "pablopio/authz" | "$KUBECTL" apply -f -
    render_manifest "$REPO_ROOT/orchestrator/ingress/ingress-authz.yaml" | "$KUBECTL" apply -f -
    [[ "$ROLLOUT" -eq 1 ]] && "$KUBECTL" -n dkms-main-ns rollout restart deployment/authz
    "$KUBECTL" -n dkms-main-ns rollout status deployment/authz --timeout="$WAIT_TIMEOUT" || true
}

apply_orchestrator() {
    echo "── apply orchestrator ──"
    render_manifest "$REPO_ROOT/orchestrator/ingress/orchestator-deployment.yaml" "pablopio/orchestator" | "$KUBECTL" apply -f -
    render_manifest "$REPO_ROOT/orchestrator/ingress/ingress-orchestator.yaml" | "$KUBECTL" apply -f -
    [[ "$ROLLOUT" -eq 1 ]] && "$KUBECTL" -n dkms-main-ns rollout restart deployment/orchestator
    "$KUBECTL" -n dkms-main-ns rollout status deployment/orchestator --timeout="$WAIT_TIMEOUT" || true
}

apply_web() {
    echo "── apply web ──"
    # web/k8s/deploy.sh honours WEB_INGRESS_HOST/WEB_IMAGE_REPO/WEB_IMAGE_TAG_STABLE.
    # Force the same INGRESS_HOST we use everywhere else to avoid the deploy
    # script auto-detecting the raw ELB hostname.
    WEB_INGRESS_HOST="$INGRESS_HOST" \
    WEB_IMAGE_TAG_STABLE="$TAG" \
    WEB_IMAGE_REPO="${WEB_IMAGE_REPO:-docker.io/${DOCKER_HUB_USERNAME:-pablopio}/dkms-web}" \
    bash "$REPO_ROOT/web/k8s/deploy.sh"
}

apply_manifests_only() {
    echo "── apply other manifests ──"
    for f in "$REPO_ROOT"/orchestrator/ingress/ingress-dkms.yaml \
             "$REPO_ROOT"/orchestrator/ingress/ingress-sdn.yaml; do
        [[ -f "$f" ]] || continue
        render_manifest "$f" | "$KUBECTL" apply -f - 2>/dev/null || true
    done
}

ensure_namespaces() {
    "$KUBECTL" get namespace dkms-main-ns >/dev/null 2>&1 || "$KUBECTL" create namespace dkms-main-ns
    "$KUBECTL" get namespace web-dkms     >/dev/null 2>&1 || "$KUBECTL" create namespace web-dkms
}

ensure_dockerhub_secret() {
    local ns="$1"
    if [[ -n "${DOCKER_HUB_USERNAME:-}" && -n "${DOCKER_HUB_TOKEN:-}" ]]; then
        "$KUBECTL" -n "$ns" create secret docker-registry dockerhub-pull \
            --docker-server="${DOCKER_HUB_SERVER:-https://index.docker.io/v1/}" \
            --docker-username="$DOCKER_HUB_USERNAME" \
            --docker-password="$DOCKER_HUB_TOKEN" \
            --docker-email="${DOCKER_HUB_EMAIL:-}" \
            --dry-run=client -o yaml | "$KUBECTL" apply -f -
    fi
}

ensure_authz_secret() {
    if [[ -z "${DB_URL:-}" || -z "${JWT_SECRET:-}" ]]; then
        echo "[k8s-apply] Warning: DB_URL or JWT_SECRET missing from env; not touching authz-secrets" >&2
        return
    fi
    "$KUBECTL" -n dkms-main-ns create secret generic authz-secrets \
        --from-literal=DB_URL="$DB_URL" \
        --from-literal=JWT_SECRET="$JWT_SECRET" \
        --from-literal=DOCKER_HUB_USERNAME="${DOCKER_HUB_USERNAME:-}" \
        --from-literal=DOCKER_HUB_TOKEN="${DOCKER_HUB_TOKEN:-}" \
        --from-literal=DOCKER_HUB_EMAIL="${DOCKER_HUB_EMAIL:-}" \
        --from-literal=DOCKER_HUB_SERVER="${DOCKER_HUB_SERVER:-https://index.docker.io/v1/}" \
        --dry-run=client -o yaml | "$KUBECTL" apply -f -
}

# Safety net: re-sync ingress hosts at the end. The web/k8s/deploy.sh path is
# stubborn about auto-detecting the raw ELB hostname when WEB_INGRESS_HOST is
# missing; this guarantees a stable canonical hostname even if something else
# clobbered it.
sync_hosts_at_end() {
    if [[ -x "$REPO_ROOT/scripts/k8s_https_enable.sh" ]]; then
        echo "── ingress host re-sync (safety net) ──"
        INGRESS_HOST="$INGRESS_HOST" bash "$REPO_ROOT/scripts/k8s_https_enable.sh" ingress-sync-host \
            2>&1 | grep -E "^\[https-enable\]|Host sincronizado" || true
        # Also fix the redirect ingress annotation (the sync script does not).
        if "$KUBECTL" -n web-dkms get ingress dkms-web-root-redirect-ingress >/dev/null 2>&1; then
            "$KUBECTL" -n web-dkms patch ingress dkms-web-root-redirect-ingress --type merge \
                -p "{\"metadata\":{\"annotations\":{\"nginx.ingress.kubernetes.io/permanent-redirect\":\"https://${INGRESS_HOST}/web/login\"}}}" \
                >/dev/null 2>&1 || true
        fi
    fi
}

ensure_namespaces
ensure_authz_secret
ensure_dockerhub_secret dkms-main-ns
ensure_dockerhub_secret web-dkms

for c in "${COMPONENTS[@]}"; do
    case "$c" in
        authz)        apply_authz ;;
        orchestrator) apply_orchestrator ;;
        web)          apply_web ;;
        manifests)    apply_manifests_only ;;
        *) echo "[k8s-apply] unknown component '$c', skipping" >&2 ;;
    esac
done

sync_hosts_at_end

echo "[k8s-apply] done."
