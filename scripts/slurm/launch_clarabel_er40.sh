#!/usr/bin/env bash
# Validation cell for the SDN Clarabel LP backend (SDN_SOLVER=clarabel).
#
# Exact reproduction of the matrix-er-n40 cell (which STALLed: microlp never
# completed a single MCMCF-λ solve at 1560 commodities, smoke 0/50, zero
# buffer fill) with the LP backend as the ONLY changed variable. Results go
# to tests/results/clarabel-er-n40/ — the matrix-er-n40 baseline is preserved.
#
# Success criterion: smoke passes (the SDN pushes rates), then the standard
# 500→16000 SAE ramp runs and gets collected like any matrix cell.
set -eu

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
NAME=clarabel-er-n40
RUN="$LUSTRE/runs/$NAME"
OUT="$LUSTRE/runs/sbatch-${NAME}-%j.out"

rm -rf "$RUN"
GEN_ARGS="--topo er --n 40 --degree 4 --pairs 8000 --key-bits 256 --qd-alpha 0"

sbatch -J "$NAME" -N 7 -c 64 --mem=180G -t 01:00:00 -p short \
    -o "$OUT" \
    --export="ALL,REPO=$REPO,RUN=$RUN,GEN_ARGS=$GEN_ARGS,SDN_SOLVER=clarabel,RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,WARMUP=30,SMOKE_RETRIES=10,SMOKE_GAP=30" \
    "$REPO/scripts/slurm/deploy.sbatch"
