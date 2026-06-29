#!/usr/bin/env bash
# Per-node agent (one Slurm task per node). Brings up THIS node's share of a
# generated deployment, phase by phase, synchronised across nodes via barrier
# files on the shared filesystem. The started binaries are children of this
# task and live inside its Slurm step cgroup, so they stay up while this agent
# runs and are cleaned up when the step ends (or by the explicit --stop).
#
# Env: RUN (run dir with plan.json + hosts.txt), WARMUP, REQS, KEEP, DURATION,
#      LAUNCH_TIMEOUT. Rank/size come from SLURM_PROCID / SLURM_NNODES.
set -u
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
RUN="${RUN:?RUN not set}"
RANK="${SLURM_PROCID:-0}"
M="${SLURM_NNODES:-1}"
LOGS="$RUN/logs"
PF="$LOGS/pids-$RANK.json"
BAR="$RUN/barrier"
MYIP="$(sed -n "$((RANK + 1))p" "$RUN/hosts.txt")"
mkdir -p "$LOGS" "$BAR"
exec >>"$LOGS/agent-$RANK.log" 2>&1   # reliable per-rank capture (srun merges/buffers oddly)
cd "$REPO"
echo "[agent] rank $RANK/$M  node $(hostname)  ip $MYIP  pid $$ $(date +%T)"

barrier() {        # barrier <name> [timeout_s]
  local name="$1" timeout="${2:-240}" d="$BAR/$1" waited=0
  mkdir -p "$d"; touch "$d/$RANK"
  while [ "$(ls "$d" 2>/dev/null | wc -l)" -lt "$M" ]; do
    sleep 1; waited=$((waited + 1))
    if [ "$waited" -ge "$timeout" ]; then
      echo "[agent] barrier '$name' TIMEOUT ($(ls "$d" | wc -l)/$M)"; return 1
    fi
  done
}

for ph in 0 1 2 3; do
  python3 scripts/slurm/launch.py --plan "$RUN/plan.json" --logs "$LOGS" \
    --only-host "$MYIP" --phase "$ph" --pids-file "$PF" \
    --timeout "${LAUNCH_TIMEOUT:-90}" --keep-going
  barrier "phase-$ph" || { echo "[agent] abort at phase $ph"; break; }
  echo "[agent] rank $RANK passed barrier phase-$ph $(date +%T)"
done
echo "[agent] rank $RANK bring-up loop complete $(date +%T)"

# ── opt-in diagnostic: snapshot listening ports over time (every rank) ──
# DUMP_PORTS=1 spawns a background watcher that records which control/data-plane
# ports are LISTENing every 60s for ~30 min, timestamped. Decisive for the
# bring-up bug: tells whether the dkms_sae ETSI ports are up at smoke time and
# whether they degrade later. Harmless when unset.
if [ "${DUMP_PORTS:-0}" = "1" ]; then
  ( for _k in $(seq 1 30); do
      echo "=== rank $RANK $(hostname) $(date +%T) ==="
      { ss -tlnp 2>/dev/null || netstat -tlnp 2>/dev/null; } \
        | grep -E ':(19[0-9]{3}|2[01][0-9]{3}|30[0-9]{3})' || echo "(no plane ports LISTEN)"
      sleep 60
    done ) > "$LOGS/ports-$RANK-watch.txt" 2>&1 &
  echo "[agent] DUMP_PORTS: port watcher started → $LOGS/ports-$RANK-watch.txt"
fi

if [ "$RANK" = "0" ]; then
  # Heartbeat: worker ranks tear down their procs once rank-0 looks idle. The
  # smoke window (SMOKE_RETRIES×SMOKE_GAP) plus the RT ramp can exceed 15 min on
  # slow SDN solves, so a FIXED worker timeout used to kill the data plane
  # mid-smoke → "Connection refused" cascade. Rank-0 now stamps a heartbeat
  # while it works; workers stay up as long as it's fresh (see the wait loop).
  ( while [ ! -f "$BAR/DONE" ]; do touch "$BAR/HEARTBEAT" 2>/dev/null; sleep 20; done ) &
  echo "[agent] warmup ${WARMUP:-12}s (buffer fill) ..."
  sleep "${WARMUP:-12}"
  # Retry smoke until it passes (or SMOKE_RETRIES exhausted). At large N the SDN
  # MCMCF-λ LP can take a long time to push the first rates; this waits for
  # buffers to fill and records time-to-first-traffic.
  retries="${SMOKE_RETRIES:-1}"; gap="${SMOKE_GAP:-30}"; t0=$(date +%s); src=1
  for i in $(seq 1 "$retries"); do
    echo "[agent] smoke attempt $i/$retries $(date +%T)"
    python3 scripts/slurm/smoke.py --plan "$RUN/plan.json" --reqs "${REQS:-5}" | tee "$LOGS/smoke.txt"
    src="${PIPESTATUS[0]}"
    [ "$src" = "0" ] && { echo "[agent] smoke PASSED after $(( $(date +%s) - t0 ))s (attempt $i)"; break; }
    [ "$i" -lt "$retries" ] && sleep "$gap"
  done
  echo "$src" > "$RUN/smoke_rc"
  if [ "${LOAD:-0}" = "1" ]; then
    echo "[agent] sustained load: dur=${LOAD_DURATION:-30}s rate=${LOAD_RATE:-20} wps=${LOAD_WPS:-1} ramp=${LOAD_RAMP:-0}"
    python3 scripts/slurm/load.py --plan "$RUN/plan.json" --out "$LOGS/load.csv" \
      --duration "${LOAD_DURATION:-30}" --rate "${LOAD_RATE:-20}" \
      --workers-per-sae "${LOAD_WPS:-1}" --ramp "${LOAD_RAMP:-0}" | tee "$LOGS/load.txt"
  fi
  # ── ETSI-014 round-trip campaign (same methodology as resultados_definitivos_n_20) ──
  # GATE: the saturation ramp only runs if the smoke (correctness) passed. This
  # realises "primero un smoke y, si todo va bien, la prueba de saturación":
  # a broken data plane (bad mTLS / routing / empty buffers) must not be loaded.
  if [ "${RT:-0}" = "1" ] && [ "$src" != "0" ]; then
    echo "[agent] smoke FAILED (rc=$src) → skipping RT saturation ramp (gate)"
  fi
  if [ "${RT:-0}" = "1" ] && [ "$src" = "0" ]; then
    W="${RT_WORKERS:-12}"
    echo "[agent] round-trip campaign: $W workers, λ=${RT_LAMBDA:-2} ramp ${RT_START:-50}+${RT_STEP:-50}/${RT_INTERVAL:-15}s hold ${RT_HOLD:-120}s"
    rm -rf "$RUN/rt"; mkdir -p "$RUN/rt"
    rtpids=()
    for w in $(seq 0 $((W - 1))); do
      python3 scripts/slurm/roundtrip.py --pairs-file "$RUN/roundtrip_pairs.json" \
        --out-dir "$RUN/rt" --worker-id "$w" --workers "$W" \
        --lambda-rps "${RT_LAMBDA:-2}" --size-bits "${RT_SIZE:-256}" \
        --warmup "${RT_WARMUP:-5}" --start-pairs "${RT_START:-50}" \
        --step-pairs "${RT_STEP:-50}" --interval "${RT_INTERVAL:-15}" --hold "${RT_HOLD:-120}" \
        ${RT_POISSON:+--poisson} \
        > "$RUN/rt/worker-$w.log" 2>&1 &
      rtpids+=($!)
    done
    # Watchdog: roundtrip.py's q.put() is a blocking enqueue, so on a starved data
    # plane (very slow SDN solve → buffers drain → requests pile up) a worker can
    # hang and never exit, burning the whole walltime with no collected result.
    # Kill any worker still alive after RT_MAX_SECONDS, then aggregate whatever was
    # written. Healthy cells finish the ramp in minutes; the cap never fires.
    rt_max="${RT_MAX_SECONDS:-1200}"
    ( sleep "$rt_max"; kill "${rtpids[@]}" 2>/dev/null; pkill -f scripts/slurm/roundtrip.py 2>/dev/null ) &
    rtwd=$!
    wait "${rtpids[@]}" 2>/dev/null
    kill "$rtwd" 2>/dev/null; wait "$rtwd" 2>/dev/null
    echo "[agent] round-trip workers done (or capped at ${rt_max}s); aggregating + plotting"
    python3 scripts/etsi014_loadtest/aggregate_workers.py "$RUN/rt" > "$RUN/rt/aggregate.log" 2>&1 || true
    for ps in plot_roundtrip plot_error_breakdown plot_match_vs_429; do
      python3 "scripts/etsi014_loadtest/$ps.py" "$RUN/rt" >> "$RUN/rt/plot.log" 2>&1 || true
    done
    echo "[agent] round-trip summary:"; cat "$RUN/rt/summary.json" 2>/dev/null || echo "  (no summary)"
  fi
  touch "$BAR/DONE"
fi

# Worker ranks hold the data plane up while rank-0 is alive. Tear down ONLY when
# rank-0 signals DONE, when rank-0's heartbeat goes stale for WORKER_GRACE s
# (rank-0 truly died), or at an absolute safety cap. This is self-tuning to any
# smoke/ramp duration — it replaces the fixed 900 s timeout that killed the data
# plane mid-smoke on slow-solve cells (the "Connection refused" root cause).
grace="${WORKER_GRACE:-300}"; capmax="${WORKER_MAX_WAIT:-7200}"; waited=0
while [ ! -f "$BAR/DONE" ]; do
  sleep 5; waited=$((waited + 5))
  if [ -f "$BAR/HEARTBEAT" ]; then
    age=$(( $(date +%s) - $(stat -c %Y "$BAR/HEARTBEAT" 2>/dev/null || echo 0) ))
    [ "$age" -ge "$grace" ] && { echo "[agent] rank-0 heartbeat stale (${age}s ≥ ${grace}s) → tear down"; break; }
  fi
  [ "$waited" -ge "$capmax" ] && { echo "[agent] worker absolute cap ${capmax}s → tear down"; break; }
done

if [ "${KEEP:-0}" = "1" ]; then
  echo "[agent] KEEP=1: holding deployment for ${DURATION:-300}s"
  sleep "${DURATION:-300}"
fi

echo "[agent] rank $RANK stopping local procs ..."
python3 scripts/slurm/launch.py --logs "$LOGS" --only-host "$MYIP" --stop --pids-file "$PF"
echo "[agent] rank $RANK done"
