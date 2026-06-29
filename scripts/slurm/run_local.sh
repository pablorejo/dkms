#!/usr/bin/env bash
# Single-node (loopback) bring-up + smoke test of a generated deployment.
# Meant to run inside a Slurm allocation (salloc/srun/sbatch) on one node.
#   Usage: run_local.sh <run_dir>     (run_dir holds plan.json from gen_deploy.py)
# Env: WARMUP (buffer-fill wait, default 10s), REQS (enc_keys per SAE, default 5),
#      LAUNCH_TIMEOUT (per-proc readiness, default 60), KEEP (1 = leave running).
set -u
RUN="${1:?usage: run_local.sh <run_dir>}"
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
LOGS="$RUN/logs"
WARMUP="${WARMUP:-10}"
REQS="${REQS:-5}"
LAUNCH_TIMEOUT="${LAUNCH_TIMEOUT:-60}"

cd "$REPO"
echo "=== node: $(hostname)  run: $RUN ==="
mkdir -p "$LOGS"

echo "── launching (phased) ..."
python3 scripts/slurm/launch.py --plan "$RUN/plan.json" --logs "$LOGS" --timeout "$LAUNCH_TIMEOUT"
rc=$?
if [ $rc -ne 0 ]; then
  echo "✗ launch failed (rc=$rc)"
  python3 scripts/slurm/launch.py --logs "$LOGS" --stop
  exit 1
fi

echo "── warmup ${WARMUP}s (buffer fill) ..."
sleep "$WARMUP"

echo "── smoke test (${REQS} enc_keys per SAE) ..."
python3 scripts/slurm/smoke.py --plan "$RUN/plan.json" --reqs "$REQS"
smoke_rc=$?

if [ "${KEEP:-0}" = "1" ]; then
  echo "── KEEP=1: leaving deployment running. Stop with:"
  echo "   python3 scripts/slurm/launch.py --logs $LOGS --stop"
else
  echo "── stopping ..."
  python3 scripts/slurm/launch.py --logs "$LOGS" --stop
fi

echo "=== run_local done: smoke_rc=$smoke_rc ==="
# Collect results into the repo's tests/results/<name>/ (gitignored).
python3 scripts/slurm/collect.py --run "$RUN" --name "$(basename "$RUN")" || true
exit $smoke_rc
