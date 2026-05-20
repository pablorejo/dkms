#!/bin/bash
# probe_n_limit.sh — encuentra el N máximo de DKMSs para el SDN
# MCMCF-λ sin OOM. Estrategia: cada N lanza una sim RGG, espera
# hasta WATCH_SECONDS, y considera OK si el SDN no ha hecho OOMKill.
#
# Uso: ./probe_n_limit.sh START_N
#  Si N OK: sube N+1, N+2, ... hasta primer OOM.
#  Si N FAIL: baja N-1, N-2, ... hasta primer OK.

set -uo pipefail

START_N=${1:-30}
WATCH_SECONDS=${WATCH_SECONDS:-600}  # 10 min de observación
ORCH="http://127.0.0.1:18080"
AUTHZ="http://127.0.0.1:18081"
STAMP=$(date -u +%Y%m%dT%H%M%SZ)
LOG_DIR=/tmp/probe-n-limit-$STAMP
mkdir -p "$LOG_DIR"
RESULTS_CSV=$LOG_DIR/results.csv
echo "N,result,sim_id,dkms_ready,sdn_restarts,sdn_oom,elapsed_s" > "$RESULTS_CSV"

log() { echo "[probe $(date -u +%H:%M:%S)] $*"; }

test_n() {
  local N=$1
  log "================ TESTING N=$N ================"
  local NAME="probe-n${N}-${STAMP}"
  local OUT=$LOG_DIR/n$N
  mkdir -p "$OUT"

  # Wipe orphan KMEs y wait clean cluster
  kubectl -n dkms-main-ns exec deploy/orchestator -- python3 -c "
from sqlalchemy import create_engine, text; import os
e=create_engine(os.environ['DB_URL'])
with e.connect() as c:
    c.execute(text('DELETE FROM kme')); c.commit()
" >/dev/null 2>&1

  cd /home/pablopio/Documentos/trabajo_atlantic/dkms_rust_feature
  # `--buffer-saturated` is required by argparse (gating flag), but with
  # `--saturation-timeout 1` it skips the actual saturation wait. We then
  # rely on `--no-stop` so the sim stays alive after dkms_topo exits.
  timeout 120 python3 -m tests.cli.dkms_topo rgg -n $N --max-distance-km 30 -k 4 --seed 42 \
    --name "$NAME" --owner 3 \
    --r0 10000 --buffer-enc-size 4096 \
    --buffer-saturated --no-stop \
    --node-id-offset 10 \
    --orch-url $ORCH --authz-url $AUTHZ \
    --username config_user --password config_password \
    --saturation-timeout 1 --pod-ready-timeout 1 --yes \
    --output-dir $OUT > $OUT/sim-launch.log 2>&1 || true

  local SIM=$(grep -oE 'sim_id=[0-9]+' $OUT/sim-launch.log | head -1 | cut -d= -f2)
  if [ -z "$SIM" ]; then
    log "  ERROR: no sim_id"
    echo "$N,FAIL_NO_SIM,,0,0,no,0" >> "$RESULTS_CSV"
    return 1
  fi
  log "  sim_id=$SIM (watching for $WATCH_SECONDS s)"

  local T_START=$(date +%s)
  local PEAK_READY=0
  local OOM_DETECTED=no
  local SDN_RESTARTS=0
  while [ $(($(date +%s) - T_START)) -lt $WATCH_SECONDS ]; do
    local READY=$(kubectl -n $SIM get pods -l app=dkms --no-headers 2>/dev/null | grep -c '4/4' || echo 0)
    [ "$READY" -gt "$PEAK_READY" ] && PEAK_READY=$READY
    local SDN_INFO=$(kubectl -n $SIM get pod -l app=sdn -o jsonpath='{range .items[0].status.containerStatuses[?(@.name=="sdn")]}{.restartCount}|{.lastState.terminated.reason}{end}' 2>/dev/null)
    SDN_RESTARTS=$(echo "$SDN_INFO" | cut -d'|' -f1)
    local REASON=$(echo "$SDN_INFO" | cut -d'|' -f2)
    if [ "$REASON" = "OOMKilled" ]; then
      OOM_DETECTED=yes
      log "  SDN OOMKilled (restarts=$SDN_RESTARTS)"
      break
    fi
    sleep 10
  done
  local ELAPSED=$(($(date +%s) - T_START))

  # Stop sim
  curl -s -X POST -H 'X-User-Id: 3' $ORCH/orch/api/sim/$SIM/stop > $OUT/stop.log 2>&1 &
  disown

  local RESULT
  if [ "$OOM_DETECTED" = "yes" ]; then
    RESULT=OOM
  elif [ "$PEAK_READY" -lt "$N" ]; then
    RESULT=PARTIAL_READY
  else
    RESULT=OK
  fi
  log "  N=$N → $RESULT (ready=$PEAK_READY/$N restarts=$SDN_RESTARTS oom=$OOM_DETECTED elapsed=${ELAPSED}s)"
  echo "$N,$RESULT,$SIM,$PEAK_READY,$SDN_RESTARTS,$OOM_DETECTED,$ELAPSED" >> "$RESULTS_CSV"

  # Wait for ns cleanup before next probe
  log "  waiting for ns $SIM cleanup..."
  for _ in $(seq 1 60); do
    kubectl get ns $SIM >/dev/null 2>&1 || break
    sleep 5
  done
  echo "$RESULT"
}

# Main loop
log "starting from N=$START_N (watch ${WATCH_SECONDS}s per probe, results $RESULTS_CSV)"
INITIAL_N=$START_N
INITIAL_RESULT=$(test_n $INITIAL_N | tail -1)
log "initial result for N=$INITIAL_N: $INITIAL_RESULT"

if [ "$INITIAL_RESULT" = "OK" ]; then
  log "going up: trying N=$((INITIAL_N+1)), $((INITIAL_N+2)), ..."
  CURRENT=$((INITIAL_N+1))
  while [ "$CURRENT" -le 50 ]; do
    RES=$(test_n $CURRENT | tail -1)
    if [ "$RES" != "OK" ]; then
      log "  N=$CURRENT failed ($RES). LIMIT = $((CURRENT-1))"
      break
    fi
    CURRENT=$((CURRENT+1))
  done
else
  log "going down: trying N=$((INITIAL_N-1)), $((INITIAL_N-2)), ..."
  CURRENT=$((INITIAL_N-1))
  while [ "$CURRENT" -ge 10 ]; do
    RES=$(test_n $CURRENT | tail -1)
    if [ "$RES" = "OK" ]; then
      log "  N=$CURRENT works. LIMIT = $CURRENT"
      break
    fi
    CURRENT=$((CURRENT-1))
  done
fi

log "=== DONE. results in $RESULTS_CSV ==="
cat "$RESULTS_CSV"
