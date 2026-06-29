#!/usr/bin/env bash
# Saturation matrix campaign: 5 topologies × N ∈ {20,30,40,50,60}, run ONE AT A
# TIME (sbatch --wait), natural SDN supply, the exact ba40-ramp recipe with N
# (and topology) as the only independent variables. Each cell auto-collects to
# tests/results/<name>/ (deploy.sbatch runs collect.py + plots at the end); we
# then extract headline metrics, stamp a DONE sentinel, and DELETE the $LUSTRE
# run dir to keep the inode footprint flat (each cell creates ~32k files —
# mostly the 16k SAE certs — and the Lustre scratch inode quota is ~250k, so
# without per-cell cleanup the campaign exhausts the quota after ~4 cells).
#
# Idempotent / resumable: re-run and it skips cells whose
# tests/results/matrix-<topo>-n<N>/DONE exists. An OUTER retry loop re-attempts
# cells that suffered an INFRASTRUCTURE failure (gen_deploy crash, node death →
# no smoke_rc file produced ⇒ not stamped DONE). A genuine SDN STALL (smoke ran
# and failed) IS a valid result and gets stamped DONE.
#
# Cell recipe (== tests/results/ba40-ramp-16000/scripts/launch_ba40.sh, over topo+N):
#   GEN  : --topo <t> --n <N> --degree <d> --pairs 8000 --key-bits 256 --qd-alpha 0
#   SUPPLY: natural MCMCF-λ (fill_rate=0), mcf_period=5000, lex-refine OFF (deploy default)
#   RAMP : RT=1, 25 workers, λ=1 Poisson, 500→16000 SAEs (+500/10s), hold 30s
#   GATE : smoke must pass (≤10×30s) before the ramp; stalled SDN fails smoke → ramp skipped
#   NODES: ~6.7 sites/site-node + 1 dedicated RT client node.
set -u

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
CAMP="$LUSTRE/runs/matrix-campaign"
mkdir -p "$CAMP"
LOG="$CAMP/progress.log"
STATUS="$CAMP/status.csv"
echo $$ > "$CAMP/driver.pid"
[ -f "$STATUS" ] || echo "ts,cell,topo,N,jobid,exit,smoke_rc,match_pct,total,p429_pct,note,round" > "$STATUS"
log(){ echo "[$(date +%F_%T)] $*" | tee -a "$LOG"; }

# cell := "fam  gen_degree  N  nodes"  (gen --topo derives from fam: ba2→ba)
cells=(
  "ba 2 20 4"  "er 4 20 4"  "ba2 4 20 4"  "rgg 4 20 4"  "secoqc 4 20 4"
  "ba 2 30 6"  "er 4 30 6"  "ba2 4 30 6"  "rgg 4 30 6"  "secoqc 4 30 6"
  "ba 2 40 7"  "er 4 40 7"  "ba2 4 40 7"  "rgg 4 40 7"  "secoqc 4 40 7"
  "ba 2 50 9"  "er 4 50 9"  "ba2 4 50 9"  "rgg 4 50 9"  "secoqc 4 50 9"
  "ba 2 60 10" "er 4 60 10" "ba2 4 60 10" "rgg 4 60 10" "secoqc 4 60 10"
)
gen_topo(){ case "$1" in ba2) echo ba ;; *) echo "$1" ;; esac; }

MAX_ROUNDS="${MAX_ROUNDS:-3}"
total_n=${#cells[@]}
log "=== matrix campaign start: $total_n cells, driver pid $$, max_rounds $MAX_ROUNDS ==="

for round in $(seq 1 "$MAX_ROUNDS"); do
  remaining=0
  for c in "${cells[@]}"; do
    read -r fam deg N nodes <<<"$c"
    name="matrix-${fam}-n${N}"
    res="$REPO/tests/results/$name"
    [ -f "$res/DONE" ] && continue
    remaining=$((remaining+1))

    gtopo="$(gen_topo "$fam")"
    RUN="$LUSTRE/runs/$name"
    rm -rf "$RUN"   # clean any partial from a prior failed attempt
    GEN_ARGS="--topo $gtopo --n $N --degree $deg --pairs 8000 --key-bits 256 --qd-alpha 0"
    log "LAUNCH $name (round $round)  nodes=$nodes  GEN_ARGS=[$GEN_ARGS]"

    out="$(sbatch --wait -J "mx-$name" -N "$nodes" -c 64 --mem=180G -t 01:00:00 -p short \
        -o "$CAMP/sbatch-${name}-%j.out" \
        --export="ALL,REPO=$REPO,RUN=$RUN,GEN_ARGS=$GEN_ARGS,RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,WARMUP=30,SMOKE_RETRIES=10,SMOKE_GAP=30" \
        "$REPO/scripts/slurm/deploy.sbatch" 2>&1)"
    ec=$?
    jid="$(printf '%s\n' "$out" | grep -oE '[0-9]+' | tail -1)"

    metrics="$(python3 "$REPO/scripts/slurm/matrix_extract.py" --results "$res" --run "$RUN" 2>/dev/null || echo '?,,,,extract-err')"
    smoke_field="$(printf '%s' "$metrics" | cut -d, -f1)"

    if [ "$smoke_field" = "?" ]; then
      # No smoke_rc file ⇒ node_agent never ran ⇒ infrastructure failure (gen_deploy
      # crash / node death / quota). Do NOT stamp DONE; retry in the next round.
      log "FAIL   $name (round $round)  exit=$ec jid=$jid  INFRA-FAIL metrics=[$metrics] — will retry"
      echo "$(date +%F_%T),$name,$fam,$N,$jid,$ec,$metrics,$round" >> "$STATUS"
    else
      log "DONE   $name (round $round)  exit=$ec jid=$jid  metrics=[$metrics]"
      echo "$(date +%F_%T),$name,$fam,$N,$jid,$ec,$metrics,$round" >> "$STATUS"
      mkdir -p "$res"; touch "$res/DONE"
    fi
    rm -rf "$RUN"   # free ~32k inodes; everything needed is already in tests/results
  done

  done_n=$(ls -d "$REPO"/tests/results/matrix-*-n*/DONE 2>/dev/null | wc -l)
  log "=== round $round complete: $done_n/$total_n DONE ==="
  [ "$done_n" -ge "$total_n" ] && break
done

done_n=$(ls -d "$REPO"/tests/results/matrix-*-n*/DONE 2>/dev/null | wc -l)
log "=== matrix campaign FINISHED: $done_n/$total_n cells DONE ==="
