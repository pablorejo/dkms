#!/usr/bin/env bash
# Unified clean scaling campaign: 5 topologies × N ∈ {10,20,…,100} = 50 cells,
# SDN_SOLVER=clarabel, SAE saturation ramp, sequential `sbatch --wait`. Replaces
# the ad-hoc matrix/clarabel/big campaigns with one well-structured grid.
#
# Includes the bring-up fix (node_agent.sh heartbeat) so slow-solve cells no
# longer hit the "Connection refused" teardown race. Cells whose SDN LP genuinely
# can't solve in the smoke window (dense N≥80, the solver ceiling) STALL cleanly.
#
# Idempotent / resumable (DONE sentinels), per-cell $LUSTRE cleanup (inode quota),
# outer retry loop for infra failures. LAUNCH DETACHED:
#   setsid nohup bash scripts/slurm/full_campaign.sh > $LUSTRE/runs/full-campaign/driver-nohup.log 2>&1 < /dev/null &
set -u

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
CAMP="$LUSTRE/runs/full-campaign"
mkdir -p "$CAMP"
LOG="$CAMP/progress.log"
STATUS="$CAMP/status.csv"
echo $$ > "$CAMP/driver.pid"
[ -f "$STATUS" ] || echo "ts,cell,topo,N,jobid,exit,smoke_rc,match_pct,total,p429_pct,note,round" > "$STATUS"
log(){ echo "[$(date +%F_%T)] $*" | tee -a "$LOG"; }

# "fam  gen_degree  N  nodes"  (nodes = ceil(N/6.7)+1, ~6.7 sites/site-node + 1 RT client).
# Ordered by N ascending: the full low-N grid lands first, the node-heavy big cells last.
cells=(
  "ba 2 10 3"   "er 4 10 3"   "ba2 4 10 3"   "rgg 4 10 3"   "secoqc 4 10 3"
  "ba 2 20 4"   "er 4 20 4"   "ba2 4 20 4"   "rgg 4 20 4"   "secoqc 4 20 4"
  "ba 2 30 6"   "er 4 30 6"   "ba2 4 30 6"   "rgg 4 30 6"   "secoqc 4 30 6"
  "ba 2 40 7"   "er 4 40 7"   "ba2 4 40 7"   "rgg 4 40 7"   "secoqc 4 40 7"
  "ba 2 50 9"   "er 4 50 9"   "ba2 4 50 9"   "rgg 4 50 9"   "secoqc 4 50 9"
  "ba 2 60 10"  "er 4 60 10"  "ba2 4 60 10"  "rgg 4 60 10"  "secoqc 4 60 10"
  "ba 2 70 12"  "er 4 70 12"  "ba2 4 70 12"  "rgg 4 70 12"  "secoqc 4 70 12"
  "ba 2 80 13"  "er 4 80 13"  "ba2 4 80 13"  "rgg 4 80 13"  "secoqc 4 80 13"
  "ba 2 90 15"  "er 4 90 15"  "ba2 4 90 15"  "rgg 4 90 15"  "secoqc 4 90 15"
  "ba 2 100 16" "er 4 100 16" "ba2 4 100 16" "rgg 4 100 16" "secoqc 4 100 16"
)
gen_topo(){ case "$1" in ba2) echo ba ;; *) echo "$1" ;; esac; }

MAX_ROUNDS="${MAX_ROUNDS:-2}"
total_n=${#cells[@]}
log "=== full campaign start: $total_n cells, driver pid $$, max_rounds $MAX_ROUNDS ==="

for round in $(seq 1 "$MAX_ROUNDS"); do
  for c in "${cells[@]}"; do
    read -r fam deg N nodes <<<"$c"
    name="full-${fam}-n${N}"
    res="$REPO/tests/results/$name"
    [ -f "$res/DONE" ] && continue

    gtopo="$(gen_topo "$fam")"
    RUN="$LUSTRE/runs/$name"
    rm -rf "$RUN"
    GEN_ARGS="--topo $gtopo --n $N --degree $deg --pairs 8000 --key-bits 256 --qd-alpha 0"
    log "LAUNCH $name (round $round)  nodes=$nodes  GEN_ARGS=[$GEN_ARGS]"

    out="$(sbatch --wait -J "full-$name" -N "$nodes" -c 64 --mem=180G -t 02:00:00 -p short \
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
    rm -rf "$RUN"   # free ~32k inodes; everything needed is already in tests/results
  done

  done_n=$(ls -d "$REPO"/tests/results/full-*-n*/DONE 2>/dev/null | wc -l)
  log "=== round $round complete: $done_n/$total_n DONE ==="
  [ "$done_n" -ge "$total_n" ] && break
done

done_n=$(ls -d "$REPO"/tests/results/full-*-n*/DONE 2>/dev/null | wc -l)
log "=== full campaign FINISHED: $done_n/$total_n cells DONE ==="
# auto-aggregate the CSVs + matrix for analysis
python3 "$REPO/scripts/slurm/full_table.py" 2>&1 | tee -a "$LOG" || true
