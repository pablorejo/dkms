#!/usr/bin/env bash
# Instrumented diagnostic run for the bring-up "Connection refused" bug.
# rgg-n70 isolates the bug from the solver ceiling (its LP DOES solve ~880s).
# DUMP_PORTS=1 → each node records LISTENing plane ports every 60s for 30 min.
# Run dir is NOT cleaned (no campaign driver), so all logs + port dumps persist
# at $LUSTRE/runs/diag-rgg-n70/ for post-mortem.
set -eu

LUSTRE=/mnt/lustre/scratch/nlsas//home/uvi/et/dca
REPO=/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust
NAME=diag-rgg-n70
RUN="$LUSTRE/runs/$NAME"

rm -rf "$RUN"
GEN_ARGS="--topo rgg --n 70 --degree 4 --pairs 8000 --key-bits 256 --qd-alpha 0"

sbatch -J "$NAME" -N 12 -c 64 --mem=180G -t 02:00:00 -p short \
    -o "$LUSTRE/runs/sbatch-${NAME}-%j.out" \
    --export="ALL,REPO=$REPO,RUN=$RUN,GEN_ARGS=$GEN_ARGS,SDN_SOLVER=clarabel,DUMP_PORTS=1,\
RT=1,RT_WORKERS=25,RT_LAMBDA=1,RT_START=10,RT_STEP=10,RT_INTERVAL=10,RT_HOLD=30,RT_POISSON=1,RT_SIZE=256,RT_WARMUP=5,\
WARMUP=30,SMOKE_RETRIES=30,SMOKE_GAP=60" \
    "$REPO/scripts/slurm/deploy.sbatch"
