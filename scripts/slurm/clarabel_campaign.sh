#!/usr/bin/env bash
# "Después" campaign for the SDN Clarabel backend: re-run the 14 cells that
# STALLed in the microlp baseline matrix (every topo at N>=40 except ba-n40,
# plus ba at N=50/60) with SDN_SOLVER=clarabel as the only changed variable.
# Names are clarabel-<fam>-n<N> so the matrix-* microlp baseline stays intact.
#
# Same skeleton as matrix_campaign.sh: sequential sbatch --wait, idempotent
# DONE sentinels under tests/results/clarabel-<fam>-n<N>/, per-cell rm -rf of
# the $LUSTRE run dir (inode quota), outer retry rounds for infra failures.
# LAUNCH DETACHED (setsid nohup ... < /dev/null &) or it dies with the session.
set -u

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
CAMP="$LUSTRE/runs/clarabel-campaign"
mkdir -p "$CAMP"
LOG="$CAMP/progress.log"
STATUS="$CAMP/status.csv"
echo $$ > "$CAMP/driver.pid"
[ -f "$STATUS" ] || echo "ts,cell,topo,N,jobid,exit,smoke_rc,match_pct,total,p429_pct,note,round" > "$STATUS"
log(){ echo "[$(date +%F_%T)] $*" | tee -a "$LOG"; }

# The 14 microlp-STALL cells: "fam  gen_degree  N  nodes"
cells=(
  "er 4 40 7"  "ba2 4 40 7"  "rgg 4 40 7"  "secoqc 4 40 7"
  "ba 2 50 9"  "er 4 50 9"   "ba2 4 50 9"  "rgg 4 50 9"  "secoqc 4 50 9"
  "ba 2 60 10" "er 4 60 10"  "ba2 4 60 10" "rgg 4 60 10" "secoqc 4 60 10"
)
gen_topo(){ case "$1" in ba2) echo ba ;; *) echo "$1" ;; esac; }

MAX_ROUNDS="${MAX_ROUNDS:-3}"
total_n=${#cells[@]}
log "=== clarabel campaign start: $total_n cells, driver pid $$, max_rounds $MAX_ROUNDS ==="

for round in $(seq 1 "$MAX_ROUNDS"); do
  for c in "${cells[@]}"; do
    read -r fam deg N nodes <<<"$c"
    name="clarabel-${fam}-n${N}"
    res="$REPO/tests/results/$name"
    [ -f "$res/DONE" ] && continue

    gtopo="$(gen_topo "$fam")"
    RUN="$LUSTRE/runs/$name"
    rm -rf "$RUN"
    GEN_ARGS="--topo $gtopo --n $N --degree $deg --pairs 8000 --key-bits 256 --qd-alpha 0"
    log "LAUNCH $name (round $round)  nodes=$nodes  GEN_ARGS=[$GEN_ARGS]"

    out="$(sbatch --wait -J "cl-$name" -N "$nodes" -c 64 --mem=180G -t 01:00:00 -p short \
        -o "$CAMP/sbatch-${name}-%j.out" \
        --export="ALL,REPO=$REPO,RUN=$RUN,GEN_ARGS=$GEN_ARGS,SDN_SOLVER=clarabel,RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,WARMUP=30,SMOKE_RETRIES=10,SMOKE_GAP=30" \
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

  done_n=$(ls -d "$REPO"/tests/results/clarabel-*-n*/DONE 2>/dev/null | wc -l)
  log "=== round $round complete: $done_n/$total_n DONE ==="
  [ "$done_n" -ge "$total_n" ] && break
done

done_n=$(ls -d "$REPO"/tests/results/clarabel-*-n*/DONE 2>/dev/null | wc -l)
log "=== clarabel campaign FINISHED: $done_n/$total_n cells DONE ==="
