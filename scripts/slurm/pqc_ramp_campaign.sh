#!/usr/bin/env bash
# Saturation-ramp campaign: ONE fixed ER N=30 base topology, sweeping the PQC
# link fraction 0.0 -> 1.0 in steps of 0.1, with the PQC edges chosen at RANDOM
# (gen_deploy --pqc-seed). Per cell: gen a --pairs (round-trip) deployment, then
# srun ramp_one_cell.sh {launch + warmup + roundtrip ramp + aggregate + analyse +
# stop}. Raw artifacts land under tests/results/<CAMP>/<cell>/; analysis/plots are
# a separate Python step (plot_pqc_ramp.py).
#
# Idempotent: a cell whose OUT_DIR already has sae_ramp_by_level.csv is skipped,
# so re-running resumes. Cells run ONE AT A TIME (one SDN alive at a time).
#
# Env: NSEEDS (random PQC draws per fraction, default 1; set 3 for mean±std bands)
#      BASE_SEED (topology seed, fixed; default 30), PAIRS (default 2000),
#      plus the RT_* / WARMUP knobs forwarded to ramp_one_cell.sh.
set -u
REPO="/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust"
cd "$REPO"
source "$LUSTRE/dkms-build/buildenv.sh" 2>/dev/null

CAMP="${CAMP:-pqc-ramp-er-n30}"
OUT="$REPO/tests/results/$CAMP"
mkdir -p "$OUT"
mkdir -p "$LUSTRE/dkms-build/camp"          # the gen.log redirect needs this to exist

TOPO=er; N=30; DEG=4
BASE_SEED="${BASE_SEED:-30}"                 # FIXED -> one base ER graph for the whole sweep
PAIRS="${PAIRS:-4000}"                       # 8000 SAEs of ramp headroom (also bounds $LUSTRE inodes: 2 PEMs/pair)
NSEEDS="${NSEEDS:-1}"                        # random PQC subsets per fraction
# Modest transport buffer (primes fast in warmup). NOTE: per-grade FILL-RATE
# enforcement is deferred in this build (the generator max-pumps to keep the
# buffer full, ignoring the SDN per-grade rate), so the DATA PLANE serves all
# single-node load without a PQC-dependent knee. The PQC effect is in the
# CONTROL PLANE: the SDN-assigned key-rate capacity (sdn_rates.csv / sdn_lambda.txt)
# spans orders of magnitude with the PQC fraction. The analysis reports both.
BUFFER_CAP="${BUFFER_CAP:-256}"
PQCS=(0.0 0.1 0.2 0.3 0.4 0.5 0.6 0.7 0.8 0.9 1.0)

# N=30: clarabel is robust here (microlp OOMs ~N>=30 on the all-pairs LP); the
# seclevels campaign verified clarabel at N=30/50.
export SDN_SOLVER="${SDN_SOLVER:-clarabel}"
export SDN_DUAL_GRADE_TABLES="${SDN_DUAL_GRADE_TABLES:-1}"
# Ramp shape (N=30 loopback). Pure SDN-driven fill (no --fill-rate floor) so the
# saturation reflects the real per-grade allocation: QKD capacitated vs PQC 1e9.
export WARMUP="${WARMUP:-100}"
export RT_WORKERS="${RT_WORKERS:-16}"
export RT_LAMBDA="${RT_LAMBDA:-2}"
export RT_START="${RT_START:-5}"
export RT_STEP="${RT_STEP:-10}"            # 16 workers x 10 / 6s -> full 8000-SAE ramp in ~140s
export RT_INTERVAL="${RT_INTERVAL:-6}"
export RT_HOLD="${RT_HOLD:-40}"
export RT_MAX_SECONDS="${RT_MAX_SECONDS:-240}"

RES="$OUT/campaign.tsv"
[ -f "$RES" ] || echo -e "cell\tpqc\tseed\tlaunch\trows\tverdict" > "$RES"

total=$(( ${#PQCS[@]} * NSEEDS )); i=0
for pqc in "${PQCS[@]}"; do
  for seed in $(seq 1 "$NSEEDS"); do
    i=$((i+1))
    name="er_n${N}_pqc${pqc}_s${seed}"
    CELL_OUT="$OUT/$name"
    if [ -f "$CELL_OUT/sae_ramp_by_level.csv" ]; then
      echo "===== [$i/$total] $name  (already done, skipping) ====="; continue
    fi
    echo "===== [$i/$total] $name  (solver=$SDN_SOLVER pairs=$PAIRS) ====="
    D="$LUSTRE/dkms-build/camp/$name"; rm -rf "$D"; mkdir -p "$CELL_OUT"

    # 1) generate the round-trip deployment with a RANDOM PQC subset (--pqc-seed)
    #    on the FIXED base topology (--seed BASE_SEED).
    if ! python3 scripts/slurm/gen_deploy.py --topo "$TOPO" --n "$N" --degree "$DEG" \
          --seed "$BASE_SEED" --pqc-fraction "$pqc" --pqc-seed "$seed" \
          --pairs "$PAIRS" --buffer-cap "$BUFFER_CAP" --hosts 127.0.0.1 --security-level qkd_prefer \
          --out "$D" > "$D.gen.log" 2>&1; then
      echo -e "$name\t$pqc\t$seed\tGEN_FAIL\t0\tGEN_FAIL" >> "$RES"
      echo "  -> GEN_FAIL"; tail -3 "$D.gen.log"; continue
    fi

    # 2) run the ramp on a compute node (one at a time)
    LOG="$CELL_OUT/run.log"
    OUT_DIR="$CELL_OUT" timeout 900 srun -p short -c32 --mem=48G -t 14 \
      --export=ALL,OUT_DIR="$CELL_OUT" \
      bash -lc "scripts/slurm/ramp_one_cell.sh $D" > "$LOG" 2>&1
    rc=$?

    launch=OK; grep -q "LAUNCH_FAIL" "$LOG" && launch=LAUNCH_FAIL
    rows=$( ( wc -l < "$CELL_OUT/requests.csv" ) 2>/dev/null || echo 0 ); rows=${rows:-0}
    verdict=OK
    [ "$launch" = "LAUNCH_FAIL" ] && verdict=LAUNCH_FAIL
    [ "$rc" = "124" ] && verdict=TIMEOUT
    [ "$rows" -le 1 ] && [ "$verdict" = "OK" ] && verdict=NO_DATA
    echo -e "$name\t$pqc\t$seed\t$launch\t$rows\t$verdict" >> "$RES"
    echo "  -> $verdict (rows=$rows launch=$launch rc=$rc)"

    rm -rf "$D"                              # free $LUSTRE; raw artifacts are in CELL_OUT
  done
done
echo "===== campaign done. results: $RES ====="
column -t -s$'\t' "$RES" 2>/dev/null || cat "$RES"
echo "Next: python3 scripts/slurm/plot_pqc_ramp.py $OUT"
