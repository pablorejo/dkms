#!/usr/bin/env bash
set -euo pipefail
LOGS=/tmp/dkms-star-demo
PIDS=$LOGS/sdn-pids
if [ ! -f "$PIDS" ] || [ ! -s "$PIDS" ]; then
    echo "── no SDN registrado en $PIDS"; exit 0
fi
while read -r pid name; do
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        echo "  ■ $name (pid $pid)"
    fi
done < "$PIDS"
: > "$PIDS"
echo "✓ SDN detenido."
