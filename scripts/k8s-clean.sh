#!/usr/bin/env bash
# Removes all DKMS-related resources from the cluster.
#
# - all simulation namespaces (numeric: 1, 2, …)
# - web-dkms namespace
# - dkms-main-ns namespace
#
# Does NOT touch: kube-system, ingress-nginx, cert-manager, default,
# AuthZ secret if you preserve it explicitly.
#
# Usage: scripts/k8s-clean.sh [--keep-control-plane]
set -euo pipefail

KEEP_CONTROL_PLANE=0
for arg in "$@"; do
    case "$arg" in
        --keep-control-plane) KEEP_CONTROL_PLANE=1 ;;
        *) echo "unknown arg: $arg" >&2; exit 2 ;;
    esac
done

KUBECTL="${KUBECTL:-kubectl}"
WAIT_TIMEOUT="${WAIT_TIMEOUT:-180s}"

current_ctx="$($KUBECTL config current-context 2>/dev/null || true)"
echo "[k8s-clean] context: ${current_ctx:-<none>}"

delete_ns() {
    local ns="$1"
    if "$KUBECTL" get namespace "$ns" >/dev/null 2>&1; then
        echo "  - delete ns/$ns"
        "$KUBECTL" delete namespace "$ns" --ignore-not-found --wait=false
    fi
}

echo "[k8s-clean] simulation namespaces (regex ^[0-9]+$)..."
mapfile -t SIM_NS < <("$KUBECTL" get namespaces -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' \
                     | awk '/^[0-9]+$/')
for ns in "${SIM_NS[@]}"; do
    [[ -n "$ns" ]] && delete_ns "$ns"
done

if [[ "$KEEP_CONTROL_PLANE" -eq 0 ]]; then
    echo "[k8s-clean] control plane namespaces..."
    delete_ns web-dkms
    delete_ns dkms-main-ns
fi

echo "[k8s-clean] waiting for namespaces to terminate..."
deadline=$(($(date +%s) + ${WAIT_TIMEOUT%s}))
while true; do
    remaining=()
    for ns in "${SIM_NS[@]:-}"; do
        [[ -n "$ns" ]] && "$KUBECTL" get namespace "$ns" >/dev/null 2>&1 && remaining+=("$ns")
    done
    if [[ "$KEEP_CONTROL_PLANE" -eq 0 ]]; then
        "$KUBECTL" get namespace web-dkms >/dev/null 2>&1 && remaining+=(web-dkms)
        "$KUBECTL" get namespace dkms-main-ns >/dev/null 2>&1 && remaining+=(dkms-main-ns)
    fi
    if [[ "${#remaining[@]}" -eq 0 ]]; then
        break
    fi
    if [[ "$(date +%s)" -ge "$deadline" ]]; then
        echo "[k8s-clean] timeout, still terminating: ${remaining[*]}" >&2
        exit 1
    fi
    sleep 3
done

echo "[k8s-clean] done."
