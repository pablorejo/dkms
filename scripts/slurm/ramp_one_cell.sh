#!/usr/bin/env bash
# Single-node (loopback) saturation RAMP of one generated deployment.
# Meant to run INSIDE a Slurm allocation (srun) on one node, driven by
# pqc_ramp_campaign.sh. Brings up the deployment, warms the buffers, then runs
# RT_WORKERS parallel roundtrip.py workers that ramp the number of active SAE
# pairs until saturation, aggregates + analyses the raw per-worker CSVs, stops,
# and copies the raw artifacts to OUT_DIR (under tests/results, on shared FS).
#
#   Usage: ramp_one_cell.sh <run_dir>      (run_dir holds plan.json + roundtrip_pairs.json)
#
# Env knobs (all optional, sensible N=30-loopback defaults):
#   OUT_DIR              where to copy raw artifacts (default tests/results/<basename>)
#   SDN_SOLVER           microlp|clarabel (default microlp; N<=30 is fine)
#   SDN_DUAL_GRADE_TABLES  1 to enable per-grade enforcement (default 1)
#   WARMUP               buffer-fill wait before the ramp, seconds (default 90)
#   LAUNCH_TIMEOUT       per-proc readiness, seconds (default 120)
#   RT_WORKERS           parallel roundtrip.py processes (default 8)
#   RT_LAMBDA            per-pair request rate, rps (default 2)
#   RT_SIZE              key size bits (default 256)
#   RT_START/RT_STEP     pairs per worker: initial / added each interval (5 / 8)
#   RT_INTERVAL          seconds between ramp steps (default 6)
#   RT_HOLD              steady hold after full ramp, seconds (default 30)
#   RT_MAX_SECONDS       ramp watchdog cap, seconds (default 300)
set -u
RUN="${1:?usage: ramp_one_cell.sh <run_dir>}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$REPO"
source "$LUSTRE/dkms-build/buildenv.sh" 2>/dev/null

LOGS="$RUN/logs"
OUT_DIR="${OUT_DIR:-$REPO/tests/results/$(basename "$RUN")}"
WARMUP="${WARMUP:-90}"
LAUNCH_TIMEOUT="${LAUNCH_TIMEOUT:-120}"
W="${RT_WORKERS:-8}"
export SDN_SOLVER="${SDN_SOLVER:-microlp}"
export SDN_DUAL_GRADE_TABLES="${SDN_DUAL_GRADE_TABLES:-1}"

mkdir -p "$LOGS" "$OUT_DIR"
echo "=== ramp cell: $(basename "$RUN")  node: $(hostname) ==="
echo "    solver=$SDN_SOLVER dual=$SDN_DUAL_GRADE_TABLES warmup=${WARMUP}s workers=$W"
echo "    ramp λ=${RT_LAMBDA:-2} start=${RT_START:-5}+step=${RT_STEP:-8}/${RT_INTERVAL:-6}s hold=${RT_HOLD:-30}s cap=${RT_MAX_SECONDS:-300}s"

# 1) bring up (phased: SDN -> quditto/QKC -> ORR -> DKMS)
python3 scripts/slurm/launch.py --plan "$RUN/plan.json" --logs "$LOGS" --timeout "$LAUNCH_TIMEOUT"
if [ $? -ne 0 ]; then
  echo "LAUNCH_FAIL"
  python3 scripts/slurm/launch.py --logs "$LOGS" --stop >/dev/null 2>&1
  exit 9
fi

# 2) warmup: buffers must fill before the ramp, or early 429s are cold-start,
#    not saturation. WARMUP must exceed one SDN MCMCF solve (a few s at N=30).
echo "── warmup ${WARMUP}s (buffer fill) ..."
sleep "$WARMUP"

# 3) saturation ramp: RT_WORKERS parallel roundtrip.py, each ramping its share
#    of pairs (worker i owns pairs[k] where k % W == i). Same spawn as node_agent.
echo "── ramp: launching $W roundtrip workers ..."
rm -rf "$RUN/rt"; mkdir -p "$RUN/rt"
rtpids=()
for w in $(seq 0 $((W - 1))); do
  python3 scripts/slurm/roundtrip.py --pairs-file "$RUN/roundtrip_pairs.json" \
    --out-dir "$RUN/rt" --worker-id "$w" --workers "$W" \
    --lambda-rps "${RT_LAMBDA:-2}" --size-bits "${RT_SIZE:-256}" \
    --warmup "${RT_WARMUP:-3}" --start-pairs "${RT_START:-5}" \
    --step-pairs "${RT_STEP:-8}" --interval "${RT_INTERVAL:-6}" --hold "${RT_HOLD:-30}" \
    --poisson \
    > "$RUN/rt/worker-$w.log" 2>&1 &
  rtpids+=($!)
done
# Watchdog: a starved data plane can make roundtrip.py's blocking enqueue hang.
# Kill any worker still alive after RT_MAX_SECONDS, then aggregate what we have.
rt_max="${RT_MAX_SECONDS:-300}"
( sleep "$rt_max"; kill "${rtpids[@]}" 2>/dev/null; pkill -f scripts/slurm/roundtrip.py 2>/dev/null ) &
rtwd=$!
wait "${rtpids[@]}" 2>/dev/null
kill "$rtwd" 2>/dev/null; wait "$rtwd" 2>/dev/null
echo "── ramp done (or capped at ${rt_max}s); aggregating + analysing"

# 4) merge per-worker CSVs (-> rt/requests.csv + rt/summary.json) and re-index
#    by active SAE count to find the saturation knee (-> sae_ramp_by_level.csv).
python3 scripts/etsi014_loadtest/aggregate_workers.py "$RUN/rt" > "$RUN/rt/aggregate.log" 2>&1 || true
python3 scripts/slurm/analyze_sae_ramp.py "$RUN/rt" > "$RUN/rt/analyze.log" 2>&1 || true

# 5) stop the deployment
python3 scripts/slurm/launch.py --logs "$LOGS" --stop >/dev/null 2>&1

# 6) extract the CONTROL-PLANE signals (compact, so we don't copy 30 big logs):
#    the SDN-assigned per-(peer,grade) fill rate is the achievable QKD/PQC key-rate
#    capacity (it varies orders of magnitude with the PQC fraction even when the
#    data plane serves everything). last sdn_rate per peer + the lambda history.
{
  echo "dkms_log,peer,enc,dec,observed_keys_per_s,sdn_rate_keys_per_s"
  for f in "$LOGS"/dkms-*.log; do
    [ -f "$f" ] || continue
    sed -E 's/\x1b\[[0-9;]*m//g' "$f" | grep "generator.state" \
      | grep -oE "peer=[^ ]+ enc=[0-9]+ dec=[0-9]+ ack_pending=[0-9]+ emit_total=[0-9]+ observed_keys_per_s=\"[0-9.]+\" sdn_rate_keys_per_s=\"[0-9.]+\"" \
      | tail -n 29 \
      | sed -E "s#^peer=([^ ]+) enc=([0-9]+) dec=([0-9]+) ack_pending=[0-9]+ emit_total=[0-9]+ observed_keys_per_s=\"([0-9.]+)\" sdn_rate_keys_per_s=\"([0-9.]+)\"#$(basename "$f"),\1,\2,\3,\4,\5#"
  done
} > "$OUT_DIR/sdn_rates.csv" 2>/dev/null || true
sed -E 's/\x1b\[[0-9;]*m//g' "$LOGS/sdn.log" 2>/dev/null | grep "recomputed" \
  | grep -oE "n_commodities=[0-9]+ n_edges=[0-9]+ lambda=[0-9.eE+-]+ flows_with_rate=[0-9]+" \
  > "$OUT_DIR/sdn_lambda.txt" 2>/dev/null || true

# 7) copy RAW artifacts to shared FS (analysis is a separate Python step)
cp -f "$RUN/plan.json"                       "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/roundtrip_pairs.json"            "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/requests.csv"                 "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/summary.json"                 "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/sae_ramp_by_level.csv"        "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/active_sae_timeseries.csv"    "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/sae_ramp.png"                 "$OUT_DIR/" 2>/dev/null || true
cp -f "$RUN/rt/analyze.log" "$RUN/rt/aggregate.log" "$OUT_DIR/" 2>/dev/null || true
cp -f "$LOGS/sdn.log" "$OUT_DIR/sdn.log" 2>/dev/null || true
# a couple of DKMS logs for the generator.state fill trace (don't copy all N)
for f in $(ls "$LOGS"/dkms-*.log 2>/dev/null | head -3); do cp -f "$f" "$OUT_DIR/" 2>/dev/null || true; done

nrows=$( ( wc -l < "$OUT_DIR/requests.csv" ) 2>/dev/null || echo 0 )
echo "=== ramp cell done: $(basename "$RUN")  rows=$nrows  -> $OUT_DIR ==="
[ "${nrows:-0}" -gt 1 ] && exit 0 || exit 1
