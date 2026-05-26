#!/usr/bin/env bash
# v8 + sync robusto + rampa larga.
#
# Cambios vs run_jitter_v8.sh:
# - SYNC_OFFSET_S=2400s (vs 1200): margen para que TODOS los workers
#   terminen bulk_provision antes del barrier (con 500 pairs cada uno).
# - PAIRS_PER_WORKER=500 (vs 200): 7500 SAEs total = 15000 rps nominal.
# - Rampa más larga: 50→500 step 50 cada 15s = 120s ramp + 90s hold = 210s.
set -uo pipefail
set +e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
BASE="${REPO_ROOT}/tests/results/etsi014-jitter-v8-synced"
LOG="${BASE}/test.log"
mkdir -p "${BASE}"

N=20
PAIRS_PER_WORKER=500
LAMBDA=2
N_WORKERS=15
STEP_PAIRS=50
INTERVAL_SECONDS=15
HOLD_SECONDS=90
WARMUP_SECONDS=5
SYNC_OFFSET_S=2400   # 40 min: margen para 500 pairs × 15 workers en provisioning serializado

log() { echo "[v8s] $(date -Is) $*" | tee -a "${LOG}"; }

log "v8-synced START — BA N=${N}, ${N_WORKERS} Jobs × ${PAIRS_PER_WORKER} pairs = $((PAIRS_PER_WORKER*N_WORKERS*2)) SAEs, λ=${LAMBDA}, sync=${SYNC_OFFSET_S}s"

# 1. Cleanup BD + jobs previos
log "preflight cleanup"
kubectl -n dkms-main-ns exec deploy/orchestator -- python3 -c "
import os, psycopg, urllib.parse as up
url = os.getenv('DB_URL', '').replace('postgresql://', '')
parsed = up.urlparse('postgresql://' + url)
conn = f'host={parsed.hostname} port={parsed.port or 5432} dbname={parsed.path.lstrip(chr(47))} user={parsed.username} password={parsed.password}'
with psycopg.connect(conn, autocommit=True) as c, c.cursor() as cur:
    cur.execute(\"UPDATE simulation SET status='finished' WHERE status='running'\")
    cur.execute('SELECT DISTINCT tls_id FROM sae WHERE tls_id IS NOT NULL')
    tls = [r[0] for r in cur.fetchall()]
    cur.execute('DELETE FROM sae')
    print(f'sae: {cur.rowcount}')
    if tls:
        cur.execute('DELETE FROM tls_config_sae WHERE id = ANY(%s)', (tls,))
    cur.execute('DELETE FROM kme')
" 2>&1 | head -3 | tee -a "${LOG}"

kubectl -n dkms-main-ns delete jobs -l app=etsi014-rt-client --ignore-not-found > /dev/null 2>&1 &
kubectl -n dkms-main-ns delete deployments -l app=etsi014-rt-client --ignore-not-found > /dev/null 2>&1 &
wait

kubectl -n dkms-main-ns exec deploy/orchestator -- python3 -c "$(cat ${REPO_ROOT}/scripts/etsi014_loadtest/bd_cleanup.py)" 2>&1 | head -5 | tee -a "${LOG}"

# 2. Sim BA N=20 + buffer test
buffer_dir="${BASE}/buffer"
sae_dir="${BASE}/sae"
mkdir -p "${buffer_dir}/data" "${buffer_dir}/orr-logs" "${sae_dir}"
hook="${REPO_ROOT}/scripts/etsi014_loadtest/patch_resources.sh"

log "buffer test"
LOGS_BASE_DIR="${buffer_dir}" python3 -m tests.cli.dkms_topo ba -n ${N} -k 3 --seed 1734 --node-id-offset ${N} \
    --name "v8s-ba-N${N}-$(date +%H%M)" \
    --buffer-saturated --no-stop \
    --buffer-enc-size 8192 \
    --saturation-timeout 600 \
    --post-run-hook "${hook}" \
    --output-dir "${buffer_dir}" \
    --orch-url http://127.0.0.1:18080 \
    --authz-url http://127.0.0.1:18081 \
    > "${buffer_dir}/run.log" 2>&1
log "buffer exit=$?"

sim_id=$(grep -m1 'sim_id=' "${buffer_dir}/run.log" | sed 's/.*sim_id=\([0-9]*\).*/\1/')
[ -z "$sim_id" ] && { log "FATAL no sim_id"; exit 3; }
log "sim_id=${sim_id}"
sat=$(grep "sat=" "${buffer_dir}/run.log" | tail -1)
log "buffer: ${sat}"

# 3. Lanzar Jobs
HOSTS_JSON=$(kubectl -n dkms-main-ns exec deploy/orchestator -- python3 -c "
import os, psycopg, urllib.parse as up, json
url = os.getenv('DB_URL', '').replace('postgresql://', '')
parsed = up.urlparse('postgresql://' + url)
conn = f'host={parsed.hostname} port={parsed.port or 5432} dbname={parsed.path.lstrip(chr(47))} user={parsed.username} password={parsed.password}'
with psycopg.connect(conn) as c, c.cursor() as cur:
    cur.execute('SELECT d.id, d.id_host FROM dkms d JOIN host h ON h.id=d.id_host WHERE h.id_simulation=${sim_id} ORDER BY d.id')
    print(json.dumps([list(r) for r in cur.fetchall()]))
" | tr -d '[:space:]')

INGRESS_HOST=$(kubectl -n "${sim_id}" get ingress -o jsonpath='{.items[0].spec.rules[0].host}')
SHARE_TAG=$(date +%s)
START_AT_TS=$(($(date +%s) + SYNC_OFFSET_S))
log "share_tag=${SHARE_TAG} start_at_ts=${START_AT_TS} (in $((START_AT_TS-$(date +%s)))s)"

kubectl -n dkms-main-ns delete configmap etsi014-rt-client-config --ignore-not-found > /dev/null 2>&1
kubectl -n dkms-main-ns create configmap etsi014-rt-client-config --from-literal=HOSTS_JSON="${HOSTS_JSON}" > /dev/null

YAML="${BASE}/jobs.yaml"
> "${YAML}"
for i in $(seq 0 $((N_WORKERS-1))); do
cat >> "${YAML}" <<YAML
---
apiVersion: batch/v1
kind: Job
metadata:
  name: etsi014-rt-client-w${i}
  namespace: dkms-main-ns
  labels: {app: etsi014-rt-client, worker_id: "${i}"}
spec:
  backoffLimit: 2
  ttlSecondsAfterFinished: 1800
  template:
    metadata: {labels: {app: etsi014-rt-client, worker_id: "${i}"}}
    spec:
      restartPolicy: OnFailure
      terminationGracePeriodSeconds: 120
      containers:
      - name: client
        image: pablopio/etsi014-rt-client:v8
        imagePullPolicy: Always
        resources:
          requests: {cpu: 500m, memory: 1Gi}
          limits:   {cpu: 2000m, memory: 2Gi}
        env:
        - {name: WORKER_ID, value: "${i}"}
        - {name: SIM_ID, value: "${sim_id}"}
        - {name: ORCH_URL, value: "http://orchestator.dkms-main-ns.svc.cluster.local:8080"}
        - {name: INGRESS_HOST, value: "${INGRESS_HOST}"}
        - {name: START_PAIRS, value: "50"}
        - {name: END_PAIRS, value: "${PAIRS_PER_WORKER}"}
        - {name: STEP_PAIRS, value: "${STEP_PAIRS}"}
        - {name: INTERVAL_SECONDS, value: "${INTERVAL_SECONDS}"}
        - {name: WARMUP_SECONDS, value: "${WARMUP_SECONDS}"}
        - {name: HOLD_SECONDS, value: "${HOLD_SECONDS}"}
        - {name: LAMBDA_RPS, value: "${LAMBDA}"}
        - {name: REQUEST_TIMEOUT, value: "5"}
        - {name: MAX_IN_FLIGHT, value: "5000"}
        - {name: OUT_DIR, value: "/var/etsi014-out"}
        - {name: SHARE_TAG, value: "${SHARE_TAG}"}
        - {name: START_AT_TS, value: "${START_AT_TS}"}
        envFrom: [{configMapRef: {name: etsi014-rt-client-config}}]
        volumeMounts: [{name: output, mountPath: /var/etsi014-out}]
      volumes: [{name: output, emptyDir: {}}]
YAML
done
kubectl apply -f "${YAML}" > /dev/null
log "${N_WORKERS} Jobs applied"

# 4. Polling DONE marker
log "polling DONE marker (timeout 60min)"
t_start=$(date +%s)
wait_timeout=3600
while true; do
    done_count=0
    failed_count=0
    for wi in $(seq 0 $((N_WORKERS-1))); do
        pod=$(kubectl -n dkms-main-ns get pod -l app=etsi014-rt-client,worker_id="${wi}" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
        if [ -z "$pod" ]; then continue; fi
        phase=$(kubectl -n dkms-main-ns get pod "$pod" -o jsonpath='{.status.phase}' 2>/dev/null)
        case "$phase" in
            Failed)    failed_count=$((failed_count+1)); continue ;;
            Succeeded) done_count=$((done_count+1)); continue ;;
        esac
        if kubectl -n dkms-main-ns exec "$pod" -- sh -c '[ -f /var/etsi014-out/DONE ]' 2>/dev/null; then
            done_count=$((done_count+1))
        fi
    done
    elapsed=$(($(date +%s) - t_start))
    log "DONE ${done_count}/${N_WORKERS} Failed=${failed_count} (elapsed ${elapsed}s)"
    if [ "$((done_count + failed_count))" -ge "$N_WORKERS" ]; then
        log "all workers DONE"
        break
    fi
    if [ "$elapsed" -gt "$wait_timeout" ]; then
        log "TIMEOUT — rescatando data igualmente"
        break
    fi
    sleep 30
done

# 5. Collect CSVs
log "collecting CSVs"
for wi in $(seq 0 $((N_WORKERS-1))); do
    pod=$(kubectl -n dkms-main-ns get pod -l app=etsi014-rt-client,worker_id="${wi}" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)
    if [ -z "$pod" ]; then log "w${wi}: NO POD"; continue; fi
    phase=$(kubectl -n dkms-main-ns get pod "$pod" -o jsonpath='{.status.phase}' 2>/dev/null)
    mkdir -p "${sae_dir}/worker-${wi}"
    kubectl -n dkms-main-ns cp "${pod}:/var/etsi014-out/requests.csv" "${sae_dir}/worker-${wi}/requests.csv" 2>/dev/null
    sz=$(stat -c '%s' "${sae_dir}/worker-${wi}/requests.csv" 2>/dev/null || echo 0)
    log "w${wi} [${phase}]: ${sz} bytes"
done

# 6. Cleanup
log "cleanup Jobs + sim"
kubectl -n dkms-main-ns delete jobs -l app=etsi014-rt-client --ignore-not-found > /dev/null 2>&1
curl -s -X POST "http://127.0.0.1:18080/orch/api/sim/${sim_id}/stop" -H "X-User-Id: 2" -o /dev/null
kubectl delete ns "${sim_id}" --wait=false > /dev/null 2>&1

# 7. Aggregate + plots
log "aggregate"
python3 "${REPO_ROOT}/scripts/etsi014_loadtest/aggregate_workers.py" "${sae_dir}" > "${sae_dir}/aggregate.log" 2>&1
python3 "${REPO_ROOT}/scripts/etsi014_loadtest/plot_roundtrip.py" "${sae_dir}" >> "${sae_dir}/aggregate.log" 2>&1
python3 "${REPO_ROOT}/scripts/etsi014_loadtest/plot_error_breakdown.py" "${sae_dir}" >> "${sae_dir}/aggregate.log" 2>&1
python3 "${REPO_ROOT}/scripts/etsi014_loadtest/plot_match_vs_429.py" "${sae_dir}" >> "${sae_dir}/aggregate.log" 2>&1

log "v8-synced END"
[ -f "${sae_dir}/summary.json" ] && python3 -c "
import json
s = json.load(open('${sae_dir}/summary.json'))
print(json.dumps(s, indent=2))
" | tee -a "${LOG}"
