#!/bin/bash
# Orquestador del benchmark de 5 topologías con N=40 DKMSs y rampa SAE
# 100→10100 step 500, λ=10 req/s usando provisión paralela (concurrency=20).
#
# Para cada topología:
#   1. Limpiar KMEs huérfanos en BD
#   2. Crear sim + buffer-saturated via dkms_topo
#   3. Cambiar owner BD a config_user (uid 3)
#   4. Lanzar parallel_loadtest (provisión paralela + tráfico)
#   5. Stop sim
#   6. Generar plots
#
# Output: tests/results/bench-5topo-<timestamp>/<topo>/
#
# Uso:
#   ./scripts/run_5_topologies_benchmark.sh [--peak 10100] [--n 40]

set -euo pipefail

PEAK=${PEAK:-10100}
N=${N:-40}
STEP=${STEP:-500}
INTERVAL=${INTERVAL:-15}
LAMBDA=${LAMBDA:-10}
CONCURRENCY=${CONCURRENCY:-20}
HOLD=${HOLD:-30}
WARMUP=${WARMUP:-10}
SAT_TIMEOUT=${SAT_TIMEOUT:-180}

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT="$ROOT/tests/results/bench-5topo-${STAMP}"
mkdir -p "$OUT"

cd "$ROOT"

ORCH="http://127.0.0.1:18080"
AUTHZ="http://127.0.0.1:18081"

log() { echo "[bench $(date -u +%H:%M:%S)] $*" | tee -a "$OUT/run.log"; }

declare -A TOPO_FLAGS=(
  ["er"]="er -n $N -k 4 --seed 42"
  ["ba"]="ba -n $N -k 4 --seed 42"
  ["rgg"]="rgg -n $N --max-distance-km 30 -k 4 --seed 42"
  ["star"]="star -p 13 -b 3"  # 1 + 13*3 = 40
  ["secoqc"]="secoqc -n $N -k 4 --seed 42"
)
declare -A TOPO_OFFSET=(
  ["er"]="10"
  ["ba"]="50"
  ["rgg"]="90"
  ["star"]="5"
  ["secoqc"]="50"
)

run_topo() {
  local topo="$1"
  local flags="${TOPO_FLAGS[$topo]}"
  local offset="${TOPO_OFFSET[$topo]}"
  local subout="$OUT/$topo"
  mkdir -p "$subout"

  log "============================================"
  log "TOPOLOGÍA: $topo  (flags: $flags, offset $offset)"
  log "============================================"

  # 1. Cleanup KMEs huérfanos en el rango del offset
  local LO=$((100000+offset))
  local HI=$((100000+offset+N+5))
  log "  cleaning orphan KMEs in range $LO..$HI"
  kubectl -n dkms-main-ns exec deploy/orchestator -- python3 -c "
import os
from sqlalchemy import create_engine, text
e = create_engine(os.environ['DB_URL'])
with e.connect() as c:
    n = c.execute(text('DELETE FROM kme WHERE local_qkc_id >= $LO AND local_qkc_id <= $HI')).rowcount
    c.commit()
    print('deleted orphan kmes:', n)
" 2>&1 | tail -2 | tee -a "$subout/cleanup.log"

  # 2. Crear sim + arrancar + esperar saturación (timeout corto, no es estricto)
  log "  launching sim with: $flags  (timeout sat=$SAT_TIMEOUT s)"
  local NAME="bench-${topo}-${STAMP}"
  set +e
  python3 -m tests.cli.dkms_topo $flags \
    --name "$NAME" \
    --owner 3 \
    --r0 2000 \
    --buffer-enc-size 4096 \
    --buffer-saturated \
    --node-id-offset $offset \
    --orch-url "$ORCH" --authz-url "$AUTHZ" \
    --username config_user --password config_password \
    --saturation-timeout "$SAT_TIMEOUT" \
    --pod-ready-timeout 300 \
    --yes \
    --output-dir "$subout" > "$subout/sim-launch.log" 2>&1 &
  CLI_PID=$!
  set -e

  # 3. Wait for sim_id to appear, capture it
  log "  waiting for sim_id..."
  local SIM_ID=""
  for _ in $(seq 1 60); do
    if grep -qE 'sim_id=[0-9]+' "$subout/sim-launch.log" 2>/dev/null; then
      SIM_ID=$(grep -oE 'sim_id=[0-9]+' "$subout/sim-launch.log" | head -1 | cut -d= -f2)
      break
    fi
    sleep 2
  done
  if [ -z "$SIM_ID" ]; then
    log "  ERROR: timed out waiting for sim_id"
    cat "$subout/sim-launch.log" | tail -20 | tee -a "$subout/cleanup.log"
    kill $CLI_PID 2>/dev/null || true
    return 1
  fi
  log "  sim_id=$SIM_ID"
  echo "$SIM_ID" > "$subout/sim_id.txt"

  # 4. Wait for ALL pods ready (skip the saturation phase if we want speed)
  log "  waiting for pods ready..."
  for _ in $(seq 1 120); do
    READY=$(kubectl -n "$SIM_ID" get pods -l app=dkms --no-headers 2>/dev/null | grep -c '4/4' || true)
    if [ "${READY:-0}" -ge "$N" ]; then
      break
    fi
    sleep 3
  done
  log "  pods ready: ${READY}/$N"

  # 5. Wait for saturation phase of dkms_topo to complete (or timeout)
  log "  waiting for saturation phase to finish..."
  for _ in $(seq 1 120); do
    if grep -qE 'sat=|ALL saturated|timeout reached' "$subout/sim-launch.log" 2>/dev/null; then
      break
    fi
    sleep 3
  done
  log "  saturation phase done"

  # 6. The dkms_topo CLI continues and tries to stop the sim. We need to
  #    SIGKILL it (not SIGTERM) so Python's `finally` block can't call
  #    stop_simulation. Otherwise the sim is gone before our parallel_loadtest
  #    starts.
  log "  SIGKILL dkms_topo CLI to keep sim alive (sim must survive finally block)"
  kill -9 $CLI_PID 2>/dev/null || true
  # Verify sim still active
  for _ in $(seq 1 5); do
    if kubectl get ns "$SIM_ID" >/dev/null 2>&1; then
      log "  sim $SIM_ID still alive ✓"
      break
    fi
    sleep 2
  done
  if ! kubectl get ns "$SIM_ID" >/dev/null 2>&1; then
    log "  ERROR: sim $SIM_ID was deleted by dkms_topo finally — skipping loadtest"
    return 1
  fi

  # 7. Launch parallel_loadtest
  log "  launching parallel_loadtest (peak=$PEAK, lambda=$LAMBDA, conc=$CONCURRENCY)"
  python3 -m tests.cli.parallel_loadtest \
    --sim-id "$SIM_ID" \
    --orch-url "$ORCH" --authz-url "$AUTHZ" \
    --username config_user --password config_password \
    --start-saes 100 --end-saes $PEAK --step-saes $STEP \
    --interval-seconds $INTERVAL --warmup $WARMUP \
    --lambda-sae $LAMBDA --concurrency $CONCURRENCY \
    --hold-seconds $HOLD \
    --output-dir "$subout" > "$subout/loadtest.log" 2>&1 || {
      log "  parallel_loadtest exited non-zero"
    }
  log "  loadtest done"
  echo "summary:" | tee -a "$subout/summary.txt"
  cat "$subout/loadtest_summary.json" 2>/dev/null | python3 -m json.tool 2>/dev/null | tee -a "$subout/summary.txt" || true

  # 8. Run extra plots
  log "  generating plots"
  python3 -m tests.cli.extra_plots "$subout" 2>&1 | tail -10 | tee -a "$subout/plots.log" || true

  # 9. Stop sim
  log "  stopping sim $SIM_ID"
  curl -s -X POST -H 'X-User-Id: 3' "$ORCH/orch/api/sim/$SIM_ID/stop" > "$subout/stop.log" 2>&1 &
  STOP_PID=$!
  # Wait up to 5 min for the stop to settle
  for _ in $(seq 1 100); do
    if ! kubectl get ns "$SIM_ID" >/dev/null 2>&1; then
      break
    fi
    sleep 3
  done
  wait $STOP_PID 2>/dev/null || true
  log "  $topo DONE"
}

log "starting 5-topology benchmark"
log "  N=$N peak=$PEAK step=$STEP interval=$INTERVAL lambda=$LAMBDA concurrency=$CONCURRENCY"
log "  output: $OUT"

TOPOS="${TOPOS:-er ba rgg star secoqc}"
log "  will run: $TOPOS"
for topo in $TOPOS; do
  if ! run_topo "$topo"; then
    log "TOPO $topo FAILED, continuing"
  fi
done

log "=== ALL DONE ==="
log "results in $OUT/"
ls -1 "$OUT"
