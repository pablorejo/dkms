#!/usr/bin/env bash
# Patches al deployment del orchestator necesarios para que aguante un
# loadtest de >30 clientes concurrentes provisionando SAEs en bulk.
#
# Sin estos patches, con 30 clientes paralelos cert issuance del endpoint
# /orch/admin/saes/bulk se serializa (uvicorn single-worker + 1 CPU) →
# batches tardan 400s + TimeoutErrors + HTTP 500.
#
# Con estos patches el provisioning de 30k SAEs baja de ~50 min a ~12 min.
#
# Patches aplicados:
# - command/args: uvicorn --workers 4 (4 procesos vs 1)
# - SQLALCHEMY_POOL_SIZE=30  (vs default 5)
# - SQLALCHEMY_MAX_OVERFLOW=60 (vs default 10) → 90 conexiones BD max
# - resources.limits: cpu=4, memory=4Gi (vs cpu=1, memory=1Gi)
# - resources.requests: cpu=1, memory=1Gi (vs cpu=200m, memory=256Mi)
#
# Idempotente: aplicar varias veces no causa daño.
set -euo pipefail

NS="${NS:-dkms-main-ns}"
DEPLOY="${DEPLOY:-orchestator}"

echo "[patch] applying loadtest patches to $NS/$DEPLOY"

kubectl -n "$NS" patch deployment "$DEPLOY" --type=strategic --patch='
spec:
  template:
    spec:
      containers:
      - name: orchestator
        command: ["uvicorn"]
        args: ["api_orchestator:app", "--host", "0.0.0.0", "--port", "8080", "--workers", "4"]
        env:
        - name: SQLALCHEMY_POOL_SIZE
          value: "30"
        - name: SQLALCHEMY_MAX_OVERFLOW
          value: "60"
        resources:
          limits:   {cpu: "4", memory: "4Gi"}
          requests: {cpu: "1", memory: "1Gi"}
'

echo "[patch] waiting for rollout..."
kubectl -n "$NS" rollout status deployment/"$DEPLOY" --timeout=120s

echo "[patch] verifying:"
kubectl -n "$NS" get deployment "$DEPLOY" -o jsonpath='{.spec.template.spec.containers[0].resources}'
echo
kubectl -n "$NS" logs deploy/"$DEPLOY" --tail=20 | grep -E "Started server process|workers"

echo
echo "[patch] DONE. Si tienes port-forward al orchestator, relánzalo (el rollout cambió el pod target)."
