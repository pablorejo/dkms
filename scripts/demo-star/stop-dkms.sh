#!/usr/bin/env bash
# Detiene los DKMSs (no toca QKC/ORR/quditto).
set -euo pipefail
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/dkms-pids

if [ ! -f "$PIDS" ] || [ ! -s "$PIDS" ]; then
    echo "── no DKMSs registrados en $PIDS"
    exit 0
fi

while read -r pid name; do
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        echo "  ■ $name (pid $pid)"
    fi
done < "$PIDS"
: > "$PIDS"
echo "✓ DKMSs detenidos."
