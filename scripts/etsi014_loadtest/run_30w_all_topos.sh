#!/usr/bin/env bash
# Itera las 5 topologías estándar (ba, er, rgg, secoqc, ba2) con el
# escenario 30w × 500p (saturación DKMS). ETA total: ~4 horas.
#
# Override TOPOS via env: TOPOS="er rgg" bash run_30w_all_topos.sh
set -uo pipefail
set +e

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
BASE="${REPO_ROOT}/tests/results/etsi014-30w-allTopos"
LOG="${BASE}/matrix.log"
mkdir -p "${BASE}"

TOPOS_DEFAULT="ba er rgg secoqc ba2"
TOPOS="${TOPOS:-$TOPOS_DEFAULT}"

log() { echo "[matrix] $(date -Is) $*" | tee -a "${LOG}"; }

log "MATRIX START — topos=(${TOPOS})"
i=0
total=$(echo "$TOPOS" | wc -w)
for topo in $TOPOS; do
    i=$((i+1))
    log "──── ${i}/${total}: ${topo} ────"
    t0=$(date +%s)

    bash "${REPO_ROOT}/scripts/etsi014_loadtest/run_30w_500p_topo.sh" "${topo}" \
        > "${BASE}/launch-${topo}.log" 2>&1

    # Mover artefactos al directorio matrix
    src="${REPO_ROOT}/tests/results/etsi014-30w-${topo}"
    dst="${BASE}/${topo}"
    if [ -d "$src" ] && [ "$src" != "$dst" ]; then
        rm -rf "$dst"
        mv "$src" "$dst"
    fi

    elapsed=$(($(date +%s) - t0))
    log "${topo} DONE in ${elapsed}s"
done

log "MATRIX END"
log "Summary:"
for topo in $TOPOS; do
    sum="${BASE}/${topo}/sae/summary.json"
    if [ -f "$sum" ]; then
        python3 -c "
import json
s = json.load(open('${sum}'))
total = s.get('total_requests', 0)
match = s.get('match_pct', 0)
http_errs = s.get('http_errors_from_server', {})
n429 = http_errs.get('enc HTTP 429', 0) + http_errs.get('dec HTTP 429', 0)
p99 = s.get('enc_latency_ms', {}).get('p99', 0)
print(f'  ${topo}: total={total:>8} match={match:6.2f}% 429={n429:>7} p99={p99:>5.0f}ms')
" 2>&1 | tee -a "${LOG}"
    else
        log "  ${topo}: NO summary"
    fi
done
