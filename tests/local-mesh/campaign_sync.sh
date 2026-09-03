#!/usr/bin/env bash
# Lleva el repo a CESGA (copia por rsync, sin .git) y deja el SHA con el que
# se compila, para que cada celda lo apunte en su meta.json.
#
#   bash tests/local-mesh/campaign_sync.sh            # rsync + GIT_SHA
#   bash tests/local-mesh/campaign_sync.sh --dry-run
set -euo pipefail
REPO="${DKMS_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
cd "$REPO"
SHA="$(git rev-parse --short HEAD)$(git diff --quiet || echo '-dirty')"
mkdir -p campaign-2026-09
echo "$SHA" > campaign-2026-09/GIT_SHA
echo "sync: HEAD=$SHA"
# Diagnóstico de la VPN en 5 s (ver memoria project-cesga-access).
if ! timeout 5 bash -c 'echo > /dev/tcp/193.144.35.12/22' 2>/dev/null; then
    echo "sync: FATAL: no llego a ft3.cesga.es:22 — ¿VPN caída?" >&2
    exit 1
fi
rsync -az ${1:-} --info=stats1 \
    --exclude=target --exclude='target-*' --exclude=.git --exclude=tests/results \
    --exclude=web --exclude=certs-results --exclude=node_modules \
    --exclude='campaign-2026-09/cells' --exclude='campaign-2026-09/slurm' \
    ./ cesga:~/dkms_rust/
echo "sync: hecho"
