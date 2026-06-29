#!/usr/bin/env bash
# Multinode deployment driver. Submit with:
#   sbatch -N <nodes> -c <cores> --mem <mem> -t <walltime> -p <part> \
#          --export=ALL,RUN=<dir>,GEN_ARGS="--topo star --branches 5 ...",\
#                   WARMUP=12,REQS=5,KEEP=0 \
#          scripts/slurm/deploy.sbatch
#
# Flow: discover the allocation's node IPs (ordered) → (re)generate the plan
# with those real IPs → launch one node_agent.sh per node. node_agent does the
# phased, barrier-synchronised bring-up + smoke (rank 0) + teardown.
#SBATCH -J dkms-deploy
set -u
# NOTE: sbatch copies this script to a spool dir, so $0 is NOT the repo path.
# REPO must come from the (exported) environment, with an absolute fallback.
REPO="${REPO:-/mnt/netapp2/Home_FT2/home/uvi/et/dca/dkms_rust}"
source "$LUSTRE/dkms-build/buildenv.sh"

RUN="${RUN:?set RUN=<run dir on \$LUSTRE>}"
GEN_ARGS="${GEN_ARGS:?set GEN_ARGS=\"--topo ... \"}"
mkdir -p "$RUN"

echo "=== allocation: $SLURM_NNODES node(s): $SLURM_NODELIST ==="
scontrol show hostnames "$SLURM_NODELIST" > "$RUN/nodes.txt"

# ordered IP list, one per node; prefer the 10.120 data network (bond0.120)
: > "$RUN/hosts.txt"
while read -r h; do
  ip="$(getent ahostsv4 "$h" 2>/dev/null | awk '{print $1}' | sort -u | grep -E '^10\.120\.' | head -1)"
  [ -z "$ip" ] && ip="$(getent ahostsv4 "$h" 2>/dev/null | awk '{print $1}' | sort -u | grep -E '^10\.' | head -1)"
  [ -z "$ip" ] && ip="$(getent hosts "$h" 2>/dev/null | awk '{print $1; exit}')"
  echo "$ip" >> "$RUN/hosts.txt"
done < "$RUN/nodes.txt"
# In round-trip mode dedicate rank-0's node entirely to the load client (no
# sites there) so the client isn't CPU-starved by co-located DKMS/ORR/QKC —
# the N=20 "definitive" campaign ran its workers on separate pods. node_agent
# rank-0 (whose IP is then absent from --hosts) launches no site procs, passes
# the phase barriers instantly, and runs the round-trip campaign.
if [ "${RT:-0}" = "1" ] && [ "$SLURM_NNODES" -gt 1 ]; then
  SITE_IPS="$(tail -n +2 "$RUN/hosts.txt" | paste -sd,)"
  echo "=== RT mode: rank0 $(head -1 "$RUN/hosts.txt") = dedicated client; sites on: $SITE_IPS ==="
else
  SITE_IPS="$(paste -sd, "$RUN/hosts.txt")"
fi
IPS="$SITE_IPS"
echo "=== site IPs: $IPS ==="

# (re)generate the plan with the real node IPs. NOTE: $RUN/tls MUST be wiped too
# — gen_pki is idempotent and would otherwise keep stale per-DKMS server certs
# whose IP SANs point at a previous allocation's node IPs, breaking peer mTLS.
rm -rf "$RUN/barrier" "$RUN/topology" "$RUN/sites" "$RUN/sdn" "$RUN/tls" "$RUN/logs"
python3 "$REPO/scripts/slurm/gen_deploy.py" $GEN_ARGS \
  --hosts "$IPS" --out "$RUN" --binaries "$CARGO_TARGET_DIR/release"

# raise fd limit for the O(N^2) ORR gRPC mesh
ulimit -n 65536 2>/dev/null || true

# bound tokio threads per process so packing many sites/node doesn't explode
# thread counts (each binary is a multi-thread tokio runtime). Override via env.
export TOKIO_WORKER_THREADS="${TOKIO_WORKER_THREADS:-2}"

# SDN MCMCF-λ LP is the O(N^2)-commodity scaling bottleneck (~10s/solve at N=20).
# Disabling the lex-refinement 2nd phase ~halves solve time and avoids microlp
# oscillation (audit rule for N>=20). Routing (BFS) is unaffected, so smoke still
# passes; this only speeds the rate-allocation LP. Override via env.
export SDN_DISABLE_LEX_REFINEMENT="${SDN_DISABLE_LEX_REFINEMENT:-1}"

# SDN LP backend: microlp (default, exact simplex — stalls at N>=40 where the
# LP never completes and smoke fails with zero buffer fill) or clarabel
# (pure-Rust interior-point, scales past that wall). Pass via
# sbatch --export="ALL,...,SDN_SOLVER=clarabel".
export SDN_SOLVER="${SDN_SOLVER:-microlp}"

# one agent per node
export RUN
srun --nodes="$SLURM_NNODES" --ntasks="$SLURM_NNODES" --ntasks-per-node=1 \
     bash "$REPO/scripts/slurm/node_agent.sh"

echo "=== deploy.sbatch done. smoke_rc=$(cat "$RUN/smoke_rc" 2>/dev/null || echo '?') ==="
cat "$RUN/logs/smoke.txt" 2>/dev/null || true

# Collect results into the repo's tests/results/<name>/ (gitignored): loose
# artifacts + ANALYSIS.md + compressed logs/configs (keeps $HOME inode use tiny).
# The live run stays on $LUSTRE; this is a copy-back of what matters.
python3 "$REPO/scripts/slurm/collect.py" --run "$RUN" --name "$(basename "$RUN")" || true
# refresh the campaign-level summary table (tests/results/SUMMARY.md + summary.csv)
python3 "$REPO/scripts/slurm/summarize.py" || true
