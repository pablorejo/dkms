#!/usr/bin/env bash
# Post-run hook v3: NO patching, solo captura de logs.
#
# El orchestator (image orchestator:buf8k-default-v1+) ya despliega DKMS
# con buffer cap=8192 + imágenes correctas leyendo env vars del propio
# deployment del orchestator (DKMS_IMAGE, ORR_IMAGE, QKC_IMAGE,
# QUDITTO_IMAGE, DKMS_BUFFER_CAPACITY_PER_PEER, etc). Por lo tanto NO
# hay que tocar los pods del sim post-launch — cualquier `kubectl set
# image|env` dispara un rollout y el rollout pierde estado in-memory
# en QKC sidecars (race de key_id mismatch contra quditto-link).
#
# Lo único que hace este hook es esperar Ready de los quditto-link y
# arrancar los log tails en background.
#
# Args:
#   $1 = sim namespace (== sim_id)
set -euo pipefail

NS="${1:?usage: $0 <sim_namespace>}"

echo "[hook] $(date -Is) waiting for DKMS pods in ns=${NS}"
i=0
while [ "$(kubectl -n "${NS}" get deploy -l app=dkms --no-headers 2>/dev/null | wc -l)" -lt 1 ]; do
    i=$((i+1))
    [ $i -gt 24 ] && { echo "[hook] timed out waiting for deployments in ${NS}"; exit 4; }
    sleep 2
done

DEPLOYS=$(kubectl -n "${NS}" get deploy -l app=dkms -o name)
N_DEPLOYS=$(echo "${DEPLOYS}" | wc -l)
echo "[hook] $(date -Is) ${N_DEPLOYS} DKMS deployments present; waiting for rollout (no patches applied)"

# El sim arranca pods con las imágenes ya correctas — no patcheamos.
# Solo esperamos a que el rollout inicial termine (creación de pods).
for d in ${DEPLOYS}; do
    kubectl -n "${NS}" rollout status "${d}" --timeout=300s > /dev/null || \
        echo "[hook] WARN: rollout for ${d} did not finish in 5m"
done

echo "[hook] $(date -Is) effective images for first dkms deployment:"
FIRST=$(echo "${DEPLOYS}" | head -1)
kubectl -n "${NS}" get "${FIRST}" -o jsonpath='{range .spec.template.spec.containers[*]}{.name}{": image="}{.image}{"\n"}{end}'

# Esperar quditto-link pods Ready. El sim los crea como Deployment pero
# su Ready no está acoplado al "saturación starts". Si los QKC empiezan
# a pedir keys antes de que quditto-link esté Ready, hay un 503 storm
# inicial que envenena las stores.
echo "[hook] $(date -Is) waiting for quditto-link pods Ready"
i=0
while true; do
    READY=$(kubectl -n "${NS}" get pod -l app=quditto-link -o jsonpath='{range .items[*]}{.status.conditions[?(@.type=="Ready")].status}{"\n"}{end}' 2>/dev/null | grep -c "^True$" || true)
    TOTAL=$(kubectl -n "${NS}" get pod -l app=quditto-link --no-headers 2>/dev/null | wc -l)
    if [ "$READY" -gt 0 ] && [ "$READY" = "$TOTAL" ]; then
        echo "[hook] quditto-link ${READY}/${TOTAL} Ready"
        break
    fi
    i=$((i+1))
    [ $i -gt 60 ] && { echo "[hook] WARN: only ${READY}/${TOTAL} quditto-link Ready after 5 min"; break; }
    sleep 5
done

# Captura de logs en background: ORR + QKC sidecars + quditto-link pods.
if [ -n "${LOGS_BASE_DIR:-}" ]; then
    mkdir -p "${LOGS_BASE_DIR}/orr-logs" "${LOGS_BASE_DIR}/qkc-logs" "${LOGS_BASE_DIR}/quditto-logs"
    PODS=$(kubectl -n "${NS}" get pod -l app=dkms -o name 2>/dev/null)
    n=0
    for pod in ${PODS}; do
        pn=$(echo "$pod" | sed 's|pod/||')
        nohup kubectl -n "${NS}" logs -f --container=orr "${pn}" \
            > "${LOGS_BASE_DIR}/orr-logs/orr-${pn}.log" 2>&1 &
        disown
        nohup kubectl -n "${NS}" logs -f --container=qkc "${pn}" \
            > "${LOGS_BASE_DIR}/qkc-logs/qkc-${pn}.log" 2>&1 &
        disown
        n=$((n+1))
    done
    QUDITTO_PODS=$(kubectl -n "${NS}" get pod -l app=quditto-link -o name 2>/dev/null)
    nq=0
    for pod in ${QUDITTO_PODS}; do
        pn=$(echo "$pod" | sed 's|pod/||')
        nohup kubectl -n "${NS}" logs -f --container=quditto "${pn}" \
            > "${LOGS_BASE_DIR}/quditto-logs/${pn}.log" 2>&1 &
        disown
        nq=$((nq+1))
    done
    echo "[hook] $(date -Is) started ${n} ORR + ${n} QKC + ${nq} quditto log tails → ${LOGS_BASE_DIR}/{orr,qkc,quditto}-logs/"
fi
