#!/usr/bin/env bash
# Large-topology probe campaign (post-Clarabel): push past the N=60 matrix.
#
# Two fronts, Clarabel backend + debouncer guard everywhere:
#  - ba tree at N=80/100: the solver is fast on trees (27 s median at N=60),
#    so these cells stress the DEPLOYMENT at scale — ORR O(N²) ML-KEM
#    bootstrap (9900 handshakes at N=100), ~600 processes, fd/port budgets.
#  - er/rgg dense at N=70/80: the solver-ceiling probe. Median solve was
#    204 s at er-n60; extrapolation says ~8-15 min at N=70/80, so the smoke
#    gate gets a 30×60 s window (vs 10×30 s) and 2 h walltime. The datum
#    "does a ramp survive on rates this stale" is the point of the probe.
#
# Same recipe as the matrix/clarabel campaigns otherwise (8000 pairs,
# 500→16000 SAEs, Poisson λ=1, natural supply). Names big-<fam>-n<N>;
# sequential sbatch --wait; DONE sentinels; per-cell $LUSTRE cleanup
# (~32k inodes/cell, quota ~250k, currently ~150k used).
# LAUNCH DETACHED (setsid nohup ... < /dev/null &) or it dies with the session.
set -u

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
CAMP="$LUSTRE/runs/big-campaign"
mkdir -p "$CAMP"
LOG="$CAMP/progress.log"
STATUS="$CAMP/status.csv"
echo $$ > "$CAMP/driver.pid"
[ -f "$STATUS" ] || echo "ts,cell,topo,N,jobid,exit,smoke_rc,match_pct,total,p429_pct,note,round" > "$STATUS"
log(){ echo "[$(date +%F_%T)] $*" | tee -a "$LOG"; }

# "fam  gen_degree  N  nodes" — ~6.7 sites/site-node + 1 RT client node.
# Ordered to learn fast: dense ceiling probe first, then tree scale-ups.
cells=(
  "er 4 70 12"
  "ba 2 80 13"
  "er 4 80 13"
  "ba 2 100 16"
  "rgg 4 70 12"
)
gen_topo(){ case "$1" in ba2) echo ba ;; *) echo "$1" ;; esac; }

MAX_ROUNDS="${MAX_ROUNDS:-2}"
total_n=${#cells[@]}
log "=== big-topology campaign start: $total_n cells, driver pid $$, max_rounds $MAX_ROUNDS ==="

for round in $(seq 1 "$MAX_ROUNDS"); do
  for c in "${cells[@]}"; do
    read -r fam deg N nodes <<<"$c"
    name="big-${fam}-n${N}"
    res="$REPO/tests/results/$name"
    [ -f "$res/DONE" ] && continue

    gtopo="$(gen_topo "$fam")"
    RUN="$LUSTRE/runs/$name"
    rm -rf "$RUN"
    GEN_ARGS="--topo $gtopo --n $N --degree $deg --pairs 8000 --key-bits 256 --qd-alpha 0"
    log "LAUNCH $name (round $round)  nodes=$nodes  GEN_ARGS=[$GEN_ARGS]"

    out="$(sbatch --wait -J "big-$name" -N "$nodes" -c 64 --mem=180G -t 02:00:00 -p short \
        -o "$CAMP/sbatch-${name}-%j.out" \
        --export="ALL,REPO=$REPO,RUN=$RUN,GEN_ARGS=$GEN_ARGS,SDN_SOLVER=clarabel,RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,WARMUP=30,SMOKE_RETRIES=30,SMOKE_GAP=60" \
        "$REPO/scripts/slurm/deploy.sbatch" 2>&1)"
    ec=$?
    jid="$(printf '%s\n' "$out" | grep -oE '[0-9]+' | tail -1)"

    metrics="$(python3 "$REPO/scripts/slurm/matrix_extract.py" --results "$res" --run "$RUN" 2>/dev/null || echo '?,,,,extract-err')"
    smoke_field="$(printf '%s' "$metrics" | cut -d, -f1)"

    if [ "$smoke_field" = "?" ]; then
      log "FAIL   $name (round $round)  exit=$ec jid=$jid  INFRA-FAIL metrics=[$metrics] — will retry"
      echo "$(date +%F_%T),$name,$fam,$N,$jid,$ec,$metrics,$round" >> "$STATUS"
    else
      log "DONE   $name (round $round)  exit=$ec jid=$jid  metrics=[$metrics]"
      echo "$(date +%F_%T),$name,$fam,$N,$jid,$ec,$metrics,$round" >> "$STATUS"
      mkdir -p "$res"; touch "$res/DONE"
    fi
    rm -rf "$RUN"   # free ~32k inodes per cell
  done

  done_n=$(ls -d "$REPO"/tests/results/big-*-n*/DONE 2>/dev/null | wc -l)
  log "=== round $round complete: $done_n/$total_n DONE ==="
  [ "$done_n" -ge "$total_n" ] && break
done

done_n=$(ls -d "$REPO"/tests/results/big-*-n*/DONE 2>/dev/null | wc -l)
log "=== big-topology campaign FINISHED: $done_n/$total_n cells DONE ==="
